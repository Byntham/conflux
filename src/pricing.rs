use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::Provider;
use crate::fsutil;
use crate::requests::{Speed, TokenUsage};

const REFRESH_EVERY: chrono::Duration = chrono::Duration::hours(24);
const TRUST_SHORTER_AFTER: chrono::Duration = chrono::Duration::days(3);
const RETRY_EVERY: Duration = Duration::from_secs(15 * 60);
const MAX_DOCUMENT: usize = 64 * 1024 * 1024;
const VENDORS: &[&str] = &["anthropic", "openai", "xai", "meta"];

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: f64,
}

impl Rates {
    fn rescaled(self, from: &Rates, to: &Rates) -> Self {
        let ratio = |a: f64, b: f64, v: f64| if a > 0.0 { v * b / a } else { b };
        Self {
            input: ratio(from.input, to.input, self.input),
            output: ratio(from.output, to.output, self.output),
            cache_read: ratio(from.cache_read, to.cache_read, self.cache_read),
            cache_write: ratio(from.cache_write, to.cache_write, self.cache_write),
            cache_write_1h: ratio(from.cache_write_1h, to.cache_write_1h, self.cache_write_1h),
        }
    }

    fn scaled(self, by: f64) -> Self {
        Self {
            input: self.input * by,
            output: self.output * by,
            cache_read: self.cache_read * by,
            cache_write: self.cache_write * by,
            cache_write_1h: self.cache_write_1h * by,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tier {
    pub above: u64,
    pub standard: Rates,
    pub fast: Option<Rates>,
    pub ultrafast: Option<Rates>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRate {
    pub tiers: Vec<Tier>,
    pub fast_multiplier: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
    pub fast: f64,
    pub savings: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.cache_read + self.cache_write + self.output + self.fast
    }

    pub fn add(&mut self, o: &Cost) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.fast += o.fast;
        self.savings += o.savings;
    }
}

fn category_cost(u: &TokenUsage, r: &Rates) -> Cost {
    let one_hour = u.cache_write_1h.min(u.cache_write);
    Cost {
        input: u.input as f64 * r.input,
        cache_read: u.cache_read as f64 * r.cache_read,
        cache_write: (u.cache_write - one_hour) as f64 * r.cache_write + one_hour as f64 * r.cache_write_1h,
        output: u.output as f64 * r.output,
        fast: 0.0,
        savings: u.cache_read as f64 * (r.input - r.cache_read).max(0.0),
    }
}

impl ModelRate {
    pub fn cost(&self, u: &TokenUsage) -> Cost {
        let prompt = u.input + u.cache_read + u.cache_write;
        let tier = self
            .tiers
            .iter()
            .find(|t| prompt > t.above)
            .or(self.tiers.last())
            .expect("a model rate has a base tier");
        let mut cost = category_cost(u, &tier.standard);
        let sped = match u.speed {
            Speed::Standard => None,
            Speed::Fast => tier.fast.or(self.fast_multiplier.map(|m| tier.standard.scaled(m))),
            Speed::Ultrafast => tier.ultrafast,
        };
        if let Some(rates) = sped {
            let sped = category_cost(u, &rates);
            cost.fast = (sped.total() - cost.total()).max(0.0);
            cost.savings = sped.savings;
        }
        cost
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Table {
    pub fetched_at: Option<DateTime<Utc>>,
    pub models: HashMap<String, ModelRate>,
}

fn num(entry: &Value, key: &str) -> Option<f64> {
    entry[key].as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}

fn read_rates(entry: &Value, suffix: &str, base: Option<&Rates>, output_fallback: Option<f64>) -> Option<Rates> {
    let input = num(entry, &format!("input_cost_per_token{suffix}"))?;
    let output = num(entry, &format!("output_cost_per_token{suffix}")).or(output_fallback)?;
    let relative = |pick: fn(&Rates) -> f64, fallback: f64| match base {
        Some(b) if b.input > 0.0 => pick(b) / b.input * input,
        _ => fallback,
    };
    let cache_write = num(entry, &format!("cache_creation_input_token_cost{suffix}"))
        .unwrap_or_else(|| relative(|r| r.cache_write, input));
    Some(Rates {
        input,
        output,
        cache_read: num(entry, &format!("cache_read_input_token_cost{suffix}"))
            .unwrap_or_else(|| relative(|r| r.cache_read, input)),
        cache_write,
        cache_write_1h: num(entry, &format!("cache_creation_input_token_cost_above_1hr{suffix}"))
            .unwrap_or_else(|| relative(|r| r.cache_write_1h, cache_write)),
    })
}

fn read_tier(entry: &Value, above: u64, suffix: &str, base: Option<&Rates>) -> Option<Tier> {
    let standard = read_rates(entry, suffix, base, base.map(|b| b.output))?;
    let sped = |speed: &str| read_rates(entry, &format!("{suffix}_{speed}"), Some(&standard), None);
    Some(Tier {
        above,
        standard,
        fast: sped("priority"),
        ultrafast: sped("ultrafast"),
    })
}

fn tier_thresholds(entry: &Value) -> Vec<u64> {
    let mut found: Vec<u64> = entry
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .filter_map(|k| k.strip_prefix("input_cost_per_token_above_")?.strip_suffix("k_tokens"))
        .filter_map(|n| n.parse::<u64>().ok())
        .collect();
    found.sort_unstable_by(|a, b| b.cmp(a));
    found.dedup();
    found
}

fn read_model(entry: &Value) -> Option<ModelRate> {
    let base = read_tier(entry, 0, "", None)?;
    let mut tiers: Vec<Tier> = tier_thresholds(entry)
        .into_iter()
        .filter_map(|k| read_tier(entry, k * 1000, &format!("_above_{k}k_tokens"), Some(&base.standard)))
        .collect();
    for tier in &mut tiers {
        let from_base = |sped: Option<Rates>| sped.map(|r| r.rescaled(&base.standard, &tier.standard));
        tier.fast = tier.fast.or(from_base(base.fast));
        tier.ultrafast = tier.ultrafast.or(from_base(base.ultrafast));
    }
    tiers.push(base);
    Some(ModelRate {
        tiers,
        fast_multiplier: entry["provider_specific_entry"]["fast"].as_f64().filter(|m| *m > 0.0),
    })
}

pub fn parse_litellm(doc: &Value) -> HashMap<String, ModelRate> {
    let mut models = HashMap::new();
    let Some(entries) = doc.as_object() else {
        return models;
    };
    for (name, entry) in entries {
        let Some(owner) = entry["litellm_provider"].as_str().filter(|p| VENDORS.contains(p)) else {
            continue;
        };
        let key = name.trim().to_lowercase();
        let (bare, prefixed) = match key.split_once('/') {
            None => (key.as_str(), false),
            Some((prefix, rest)) if prefix == owner && !rest.contains('/') => (rest, true),
            Some(_) => continue,
        };
        let id = format!("{owner}/{bare}");
        if (prefixed || !models.contains_key(&id))
            && let Some(rate) = read_model(entry)
        {
            models.insert(id, rate);
        }
    }
    models
}

fn vendor(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "anthropic",
        Provider::Codex => "openai",
        Provider::Grok => "xai",
        Provider::Muse => "meta",
    }
}

fn without_date(name: &str) -> Option<&str> {
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let (head, tail) = name.rsplit_once('-')?;
    if digits(tail, 8) {
        return Some(head);
    }
    let (head, month) = head.rsplit_once('-')?;
    let (head, year) = head.rsplit_once('-')?;
    (digits(tail, 2) && digits(month, 2) && digits(year, 4)).then_some(head)
}

pub fn candidates(provider: Provider, model: &str) -> Vec<String> {
    let vendor = vendor(provider);
    let mut name = model.trim().to_lowercase();
    if let Some(open) = name.find('[') {
        name.truncate(open);
    }
    if let Some(rest) = name.strip_prefix(&format!("{vendor}/")) {
        name = rest.to_string();
    }
    let mut names = vec![name.clone()];
    if let Some(undated) = without_date(&name) {
        names.push(undated.to_string());
    }
    for n in names.clone() {
        if let Some(base) = n.strip_suffix("-build") {
            names.push(base.to_string());
        }
    }
    names.into_iter().map(|n| format!("{vendor}/{n}")).collect()
}

impl Table {
    pub fn rate(&self, provider: Provider, model: &str) -> Option<&ModelRate> {
        candidates(provider, model).iter().find_map(|n| self.models.get(n))
    }
}

pub fn download_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()
        .expect("build price client")
}

pub struct Prices {
    table: RwLock<Arc<Table>>,
    file: Option<PathBuf>,
    url: String,
}

impl Prices {
    pub fn new(url: String, file: Option<PathBuf>) -> Self {
        let table = file
            .as_deref()
            .and_then(|f| fsutil::read_json::<Table>(f).ok().flatten())
            .unwrap_or_default();
        Self {
            table: RwLock::new(Arc::new(table)),
            file,
            url,
        }
    }

    pub fn table(&self) -> Arc<Table> {
        self.table.read().unwrap().clone()
    }

    fn due(&self, now: DateTime<Utc>) -> bool {
        self.table().fetched_at.is_none_or(|t| now - t >= REFRESH_EVERY)
    }

    pub async fn refresh(&self, http: &reqwest::Client) -> anyhow::Result<()> {
        let mut resp = http.get(&self.url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            bytes.extend_from_slice(&chunk);
            anyhow::ensure!(bytes.len() <= MAX_DOCUMENT, "the price list is larger than expected");
        }
        let models = parse_litellm(&serde_json::from_slice(&bytes)?);
        let current = self.table();
        let known = current.models.len();
        let aged = current.fetched_at.is_none_or(|t| Utc::now() - t > TRUST_SHORTER_AFTER);
        anyhow::ensure!(
            !models.is_empty() && (aged || models.len() * 2 >= known),
            "the price list has {} models, against {known} before",
            models.len()
        );
        let table = Table {
            fetched_at: Some(Utc::now()),
            models,
        };
        let table = Arc::new(table);
        *self.table.write().unwrap() = table.clone();
        if let Some(file) = &self.file
            && let Err(e) = fsutil::write_json_durable(file, &*table)
        {
            tracing::warn!("could not save API prices: {e:#}");
        }
        Ok(())
    }

    pub async fn run(self: Arc<Self>, http: reqwest::Client) {
        loop {
            if self.due(Utc::now())
                && let Err(e) = self.refresh(&http).await
            {
                tracing::warn!("could not fetch API prices: {e:#}");
            }
            tokio::time::sleep(RETRY_EVERY).await;
        }
    }
}

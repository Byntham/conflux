use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, DurationRound, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::Provider;
use crate::app::App;
use crate::fsutil;
use crate::pricing::{Cost, ModelRate, Table};
use crate::requests::{RequestRecord, TokenUsage};

const KEEP_DAYS: i64 = 92;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub at: DateTime<Utc>,
    pub machine: String,
    pub provider: Provider,
    pub account: String,
    pub model: Option<String>,
    pub session: Option<String>,
    pub status: u16,
    pub usage: TokenUsage,
}

impl From<&RequestRecord> for Row {
    fn from(r: &RequestRecord) -> Self {
        Self {
            at: r.at,
            machine: r.machine.clone(),
            provider: r.provider,
            account: r.account.clone(),
            model: r.model.clone(),
            session: r.session.clone(),
            status: r.status,
            usage: r.usage,
        }
    }
}

pub struct Ledger {
    dir: PathBuf,
    writing: Mutex<()>,
}

fn day_file(dir: &std::path::Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{}.jsonl", day.format("%Y-%m-%d")))
}

impl Ledger {
    pub fn open(dir: PathBuf) -> anyhow::Result<Arc<Self>> {
        fsutil::ensure_private_dir(&dir)?;
        Ok(Arc::new(Self {
            dir,
            writing: Mutex::new(()),
        }))
    }

    pub fn append(&self, row: &Row) {
        if let Err(e) = self.try_append(row) {
            tracing::warn!("could not record usage: {e:#}");
        }
    }

    fn try_append(&self, row: &Row) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(row)?;
        line.push(b'\n');
        let path = day_file(&self.dir, row.at.date_naive());
        let _writing = self.writing.lock().unwrap();
        if !path.exists() {
            self.prune(row.at);
        }
        fsutil::open_private_append(&path)?.write_all(&line)?;
        Ok(())
    }

    fn prune(&self, now: DateTime<Utc>) {
        let oldest = (now - Duration::days(KEEP_DAYS)).date_naive();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let day = name
                .to_str()
                .and_then(|n| n.strip_suffix(".jsonl"))
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            if day.is_some_and(|d| d < oldest) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub fn read(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut day = since.date_naive();
        while day <= until.date_naive() {
            if let Ok(file) = std::fs::File::open(day_file(&self.dir, day)) {
                rows.extend(
                    BufReader::new(file)
                        .split(b'\n')
                        .map_while(Result::ok)
                        .filter_map(|line| serde_json::from_slice::<Row>(&line).ok())
                        .filter(|r| r.at >= since && r.at <= until),
                );
            }
            day = day.succ_opt().unwrap_or(day);
            if day == NaiveDate::MAX {
                break;
            }
        }
        rows
    }
}

#[derive(Default)]
struct Group {
    requests: u64,
    no_usage: u64,
    sessions: HashSet<u64>,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    cost: Cost,
    priced_tokens: u64,
    unpriced_tokens: u64,
}

fn tokens(u: &TokenUsage) -> u64 {
    u.input + u.cache_read + u.cache_write + u.output
}

impl Group {
    fn add(&mut self, row: &Row, session: Option<u64>, cost: Option<&Cost>) {
        let u = &row.usage;
        let n = tokens(u);
        self.requests += 1;
        if n == 0 && row.status < 400 {
            self.no_usage += 1;
        }
        if let Some(s) = session {
            self.sessions.insert(s);
        }
        self.input += u.input;
        self.cache_read += u.cache_read;
        self.cache_write += u.cache_write;
        self.output += u.output;
        match cost {
            Some(c) => {
                self.cost.add(c);
                self.priced_tokens += n;
            }
            None => self.unpriced_tokens += n,
        }
    }

    fn view(&self) -> Value {
        let cost = (self.priced_tokens > 0 || self.unpriced_tokens == 0).then(|| {
            json!({
                "total": self.cost.total(),
                "input": self.cost.input,
                "cache_read": self.cost.cache_read,
                "cache_write": self.cost.cache_write,
                "output": self.cost.output,
                "fast": self.cost.fast,
                "savings": self.cost.savings,
            })
        });
        json!({
            "requests": self.requests,
            "no_usage": self.no_usage,
            "sessions": self.sessions.len(),
            "input": self.input,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "output": self.output,
            "tokens": self.input + self.cache_read + self.cache_write + self.output,
            "unpriced_tokens": self.unpriced_tokens,
            "cost": cost,
        })
    }
}

fn grouped<K: Ord>(groups: BTreeMap<K, Group>, mut fields: impl FnMut(&K) -> Value) -> Vec<Value> {
    let mut out: Vec<(f64, u64, Value)> = groups
        .into_iter()
        .map(|(k, g)| {
            let mut v = g.view();
            if let (Value::Object(v), Value::Object(f)) = (&mut v, fields(&k)) {
                v.extend(f);
            }
            (g.cost.total(), g.input + g.cache_read + g.cache_write + g.output, v)
        })
        .collect();
    out.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    out.into_iter().map(|(_, _, v)| v).collect()
}

pub struct Filter<'a> {
    pub machine: Option<&'a str>,
    pub provider: Option<Provider>,
}

pub fn summarize(rows: &[Row], prices: &Table, filter: &Filter, email: impl Fn(&str) -> Option<String>) -> Value {
    let mut machines = HashSet::new();
    let mut providers = HashSet::new();
    let mut total = Group::default();
    let mut by_provider: BTreeMap<Provider, Group> = BTreeMap::new();
    let mut by_model: BTreeMap<(Provider, &str), Group> = BTreeMap::new();
    let mut by_account: BTreeMap<(Provider, &str), Group> = BTreeMap::new();
    let mut by_machine: BTreeMap<&str, Group> = BTreeMap::new();
    let mut hours: BTreeMap<(DateTime<Utc>, Provider), (u64, f64)> = BTreeMap::new();
    let mut rates: HashMap<(Provider, &str), Option<&ModelRate>> = HashMap::new();
    let mut unpriced_models = HashSet::new();
    for row in rows {
        machines.insert(row.machine.as_str());
        providers.insert(row.provider);
        if filter.machine.is_some_and(|m| m != row.machine) || filter.provider.is_some_and(|p| p != row.provider) {
            continue;
        }
        let model = row.model.as_deref().unwrap_or("unknown");
        let rate = *rates
            .entry((row.provider, model))
            .or_insert_with(|| prices.rate(row.provider, model));
        let cost = rate.map(|rate| rate.cost(&row.usage));
        if cost.is_none() && tokens(&row.usage) > 0 {
            unpriced_models.insert(model);
        }
        let session = row.session.as_deref().map(|s| {
            let mut h = DefaultHasher::new();
            (row.provider, s).hash(&mut h);
            h.finish()
        });
        let cost = cost.as_ref();
        total.add(row, session, cost);
        by_provider.entry(row.provider).or_default().add(row, session, cost);
        by_model
            .entry((row.provider, model))
            .or_default()
            .add(row, session, cost);
        by_account
            .entry((row.provider, row.account.as_str()))
            .or_default()
            .add(row, session, cost);
        by_machine
            .entry(row.machine.as_str())
            .or_default()
            .add(row, session, cost);
        let hour = row.at.duration_trunc(Duration::hours(1)).unwrap_or(row.at);
        let bucket = hours.entry((hour, row.provider)).or_default();
        bucket.0 += tokens(&row.usage);
        bucket.1 += cost.map_or(0.0, Cost::total);
    }
    let mut machines: Vec<&str> = machines.into_iter().collect();
    machines.sort_unstable();
    let mut providers: Vec<Provider> = providers.into_iter().collect();
    providers.sort();
    let mut unpriced_models: Vec<&str> = unpriced_models.into_iter().collect();
    unpriced_models.sort();
    json!({
        "machines": machines,
        "providers": providers,
        "unpriced_models": unpriced_models,
        "total": total.view(),
        "by_provider": grouped(by_provider, |p| json!({ "provider": p })),
        "by_model": grouped(by_model, |(p, m)| json!({ "provider": p, "model": m })),
        "by_account": grouped(by_account, |(p, a)| json!({ "provider": p, "account": a, "email": email(a) })),
        "by_machine": grouped(by_machine, |m| json!({ "machine": m })),
        "hours": hours
            .into_iter()
            .map(|((at, provider), (tokens, cost))| json!({ "at": at, "provider": provider, "tokens": tokens, "cost": cost }))
            .collect::<Vec<_>>(),
    })
}

#[derive(Deserialize)]
pub struct UsageQuery {
    range: Option<String>,
    machine: Option<String>,
    provider: Option<String>,
}

fn range_hours(range: &str) -> Option<i64> {
    match range {
        "24h" => Some(24),
        "7d" => Some(7 * 24),
        "30d" => Some(30 * 24),
        "90d" => Some(90 * 24),
        _ => None,
    }
}

pub async fn usage(State(app): State<Arc<App>>, Query(q): Query<UsageQuery>) -> Response {
    let range = q.range.unwrap_or_else(|| "7d".into());
    let Some(hours) = range_hours(&range) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "range must be 24h, 7d, 30d, or 90d"})),
        )
            .into_response();
    };
    let provider = match q.provider.as_deref().filter(|p| !p.is_empty()) {
        None => None,
        Some(p) => match serde_json::from_value::<Provider>(json!(p)) {
            Ok(p) => Some(p),
            Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "unknown provider"}))).into_response(),
        },
    };
    let Some(ledger) = app.requests.ledger() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "usage history is off"})),
        )
            .into_response();
    };
    let now = Utc::now();
    let since = now - Duration::hours(hours);
    let prices = app.prices.table();
    let emails: HashMap<String, Option<String>> = app
        .store
        .all()
        .iter()
        .map(|e| (e.id.clone(), e.credentials().email))
        .collect();
    let machine = q.machine.filter(|m| !m.is_empty());
    let summary = tokio::task::spawn_blocking(move || {
        let rows = ledger.read(since, now);
        let filter = Filter {
            machine: machine.as_deref(),
            provider,
        };
        summarize(&rows, &prices, &filter, |id| emails.get(id).cloned().flatten())
    })
    .await;
    match summary {
        Ok(mut v) => {
            v["range"] = json!(range);
            v["since"] = json!(since);
            v["now"] = json!(now);
            v["prices_fetched_at"] = json!(app.prices.table().fetched_at);
            Json(v).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

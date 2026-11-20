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
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::Provider;
use crate::app::App;
use crate::fsutil;
use crate::pricing::{Cost, ModelRate, Table};
use crate::requests::{RequestRecord, TokenUsage};

const KEEP_DAYS: i64 = 92;
const MAX_BUCKETS: usize = 400;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub at: DateTime<Utc>,
    pub machine: String,
    pub provider: Provider,
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    pub usage: TokenUsage,
}

impl From<&RequestRecord> for Row {
    fn from(r: &RequestRecord) -> Self {
        Self {
            at: r.at,
            machine: r.machine.clone(),
            provider: r.provider,
            account: r.account.clone(),
            email: r.email.clone(),
            model: r.model.clone(),
            session: r.session.clone(),
            usage: r.usage,
        }
    }
}

pub struct Ledger {
    dir: PathBuf,
    writing: Mutex<()>,
    last_error: Mutex<Option<String>>,
}

fn day_file(dir: &std::path::Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{}.jsonl", day.format("%Y-%m-%d")))
}

impl Ledger {
    pub fn open(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            dir,
            writing: Mutex::new(()),
            last_error: Mutex::new(None),
        })
    }

    pub fn append(&self, row: &Row) {
        let failure = self.try_append(row).err().map(|e| format!("{e:#}"));
        let mut last = self.last_error.lock().unwrap();
        if let Some(e) = &failure
            && last.as_ref() != Some(e)
        {
            tracing::warn!("could not record usage: {e}");
        }
        *last = failure;
    }

    fn try_append(&self, row: &Row) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(row)?;
        line.push(b'\n');
        let path = day_file(&self.dir, row.at.date_naive());
        let _writing = self.writing.lock().unwrap();
        if !path.exists() {
            fsutil::ensure_private_dir(&self.dir)?;
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

    pub fn read(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> std::io::Result<Vec<Row>> {
        let missing = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        match std::fs::metadata(&self.dir) {
            Err(e) if missing(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
            Ok(m) if !m.is_dir() => {
                return Err(std::io::Error::other(format!("{} is not a folder", self.dir.display())));
            }
            Ok(_) => {}
        }
        let mut rows = Vec::new();
        let mut day = since.date_naive();
        while day <= until.date_naive() {
            match std::fs::File::open(day_file(&self.dir, day)) {
                Ok(file) => {
                    for line in BufReader::new(file).split(b'\n') {
                        if let Ok(r) = serde_json::from_slice::<Row>(&line?)
                            && r.at >= since
                            && r.at <= until
                        {
                            rows.push(r);
                        }
                    }
                }
                Err(e) if missing(&e) => {}
                Err(e) => return Err(e),
            }
            day = day.succ_opt().unwrap_or(day);
            if day == NaiveDate::MAX {
                break;
            }
        }
        Ok(rows)
    }
}

#[derive(Default)]
struct Group {
    requests: u64,
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
    pub starts: &'a [DateTime<Utc>],
}

pub fn summarize(rows: &[Row], prices: &Table, filter: &Filter, email: impl Fn(&str) -> Option<String>) -> Value {
    let mut machines = HashSet::new();
    let mut providers = HashSet::new();
    let mut total = Group::default();
    let mut by_provider: BTreeMap<Provider, Group> = BTreeMap::new();
    let mut by_model: BTreeMap<(Provider, &str), Group> = BTreeMap::new();
    let mut by_account: BTreeMap<(Provider, &str), Group> = BTreeMap::new();
    let mut by_machine: BTreeMap<&str, Group> = BTreeMap::new();
    let mut buckets: BTreeMap<(usize, Provider), (u64, f64, u64)> = BTreeMap::new();
    let mut rates: HashMap<(Provider, &str), Option<&ModelRate>> = HashMap::new();
    let mut unpriced_models = HashSet::new();
    let mut recorded_emails: HashMap<&str, &str> = HashMap::new();
    for row in rows {
        if let Some(e) = &row.email {
            recorded_emails.insert(row.account.as_str(), e.as_str());
        }
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
        if let Some(i) = filter.starts.partition_point(|s| *s <= row.at).checked_sub(1) {
            let bucket = buckets.entry((i, row.provider)).or_default();
            bucket.0 += tokens(&row.usage);
            match cost {
                Some(c) => bucket.1 += c.total(),
                None => bucket.2 += tokens(&row.usage),
            }
        }
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
        "by_account": grouped(by_account, |(p, a)| json!({ "provider": p, "account": a, "email": email(a).or_else(|| recorded_emails.get(a).map(|e| e.to_string())) })),
        "by_machine": grouped(by_machine, |m| json!({ "machine": m })),
        "buckets": buckets
            .into_iter()
            .map(|((i, provider), (tokens, cost, unpriced))| {
                json!({ "i": i, "provider": provider, "tokens": tokens, "cost": cost, "unpriced_tokens": unpriced })
            })
            .collect::<Vec<_>>(),
    })
}

#[derive(Deserialize)]
pub struct UsageQuery {
    buckets: String,
    machine: Option<String>,
    provider: Option<String>,
}

fn bucket_starts(raw: &str, now: DateTime<Utc>) -> Option<Vec<DateTime<Utc>>> {
    let starts: Vec<DateTime<Utc>> = raw
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<i64>()
                .ok()
                .and_then(|t| DateTime::from_timestamp(t, 0))
        })
        .collect::<Option<_>>()?;
    let ordered = starts.windows(2).all(|w| w[0] < w[1]);
    let (first, last) = (*starts.first()?, *starts.last()?);
    let recent = first >= now - Duration::days(KEEP_DAYS) && last <= now + Duration::hours(1);
    (ordered && starts.len() <= MAX_BUCKETS && recent).then_some(starts)
}

pub async fn usage(State(app): State<Arc<App>>, Query(q): Query<UsageQuery>) -> Response {
    let now = Utc::now();
    let Some(starts) = bucket_starts(&q.buckets, now) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "buckets must be up to 400 increasing Unix times from the last 92 days"})),
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
    let ledger = app.requests.ledger();
    let since = starts[0];
    let prices = app.prices.table();
    let fetched_at = prices.fetched_at;
    let emails: HashMap<String, Option<String>> = app
        .store
        .all()
        .iter()
        .map(|e| (e.id.clone(), e.credentials().email))
        .collect();
    let machine = q.machine.filter(|m| !m.is_empty());
    let summary = tokio::task::spawn_blocking(move || {
        let rows = ledger.read(since, now)?;
        let filter = Filter {
            machine: machine.as_deref(),
            provider,
            starts: &starts,
        };
        Ok::<_, std::io::Error>(summarize(&rows, &prices, &filter, |id| {
            emails.get(id).cloned().flatten()
        }))
    })
    .await;
    match summary {
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("the hub could not read its usage history: {e}")})),
        )
            .into_response(),
        Ok(Ok(mut v)) => {
            v["now"] = json!(now);
            v["prices_fetched_at"] = json!(fetched_at);
            Json(v).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

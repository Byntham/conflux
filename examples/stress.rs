mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::{any, get, post};
use chrono::{DateTime, Utc};
use clap::Parser;
use common::{Layout, add_accounts, build_conflux, endpoint_env, stop_requested};
use conflux::accounts::Provider;
use conflux::config::Endpoints;
use conflux::oauth::LoginTokens;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode};
use rand::Rng;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Parser, Clone)]
#[command(about = "Drive many fake accounts and sessions through a throwaway conflux hub and node")]
struct Args {
    #[arg(long, default_value_t = 12)]
    claude_accounts: usize,
    #[arg(long, default_value_t = 8)]
    codex_accounts: usize,
    #[arg(long, default_value_t = 40)]
    claude_sessions: usize,
    #[arg(long, default_value_t = 15)]
    codex_sessions: usize,
    #[arg(long, default_value_t = 15)]
    ws_sessions: usize,
    #[arg(long, default_value_t = 60)]
    secs: u64,
    #[arg(long, default_value_t = 200)]
    turn_min_ms: u64,
    #[arg(long, default_value_t = 2000)]
    turn_max_ms: u64,
    #[arg(long, default_value_t = 500)]
    think_ms: u64,
    #[arg(
        long,
        default_value_t = 60.0,
        help = "turns an account takes per short window, give or take half"
    )]
    short_cap: f64,
    #[arg(
        long,
        default_value_t = 300.0,
        help = "turns an account takes per long window, give or take half"
    )]
    long_cap: f64,
    #[arg(long, default_value_t = 60)]
    short_secs: i64,
    #[arg(long, default_value_t = 600)]
    long_secs: i64,
    #[arg(long, default_value_t = 0.0, help = "chance a turn gets a 529 or 500")]
    error_rate: f64,
    #[arg(
        long,
        default_value_t = 0.0,
        help = "chance a turn gets a short 429 with retry-after"
    )]
    throttle_rate: f64,
    #[arg(long, default_value_t = 0.0, help = "chance a stream is cut halfway")]
    drop_rate: f64,
    #[arg(
        long,
        default_value_t = 0.0,
        help = "chance a request finds its access token revoked"
    )]
    revoke_rate: f64,
    #[arg(long, help = "turns one account runs at once before it answers with a short 429")]
    max_concurrent: Option<u64>,
    #[arg(long, default_value_t = 300)]
    refresh_ms: u64,
    #[arg(long, help = "send load straight to the hub instead of through the node")]
    direct: bool,
}

const THROTTLE_SECS: i64 = 3;

struct Window {
    used: f64,
    cap: f64,
    period: chrono::Duration,
    resets: DateTime<Utc>,
}

impl Window {
    fn roll(&mut self, now: DateTime<Utc>) {
        while now >= self.resets {
            self.used = 0.0;
            self.resets += self.period;
        }
    }

    fn full(&self) -> bool {
        self.used >= self.cap
    }

    fn pct(&self) -> f64 {
        (self.used / self.cap * 100.0).min(100.0)
    }
}

#[derive(Default, Clone)]
struct AccountStats {
    served: u64,
    refused: u64,
    throttled: u64,
    errors: u64,
    dropped: u64,
    revoked: u64,
    stale_token: u64,
    refreshes: u64,
    peak_in_flight: u64,
}

struct Account {
    provider: Provider,
    generation: u64,
    revoked: bool,
    short: Window,
    long: Window,
    blocked_until: DateTime<Utc>,
    in_flight: u64,
    told_full: Option<DateTime<Utc>>,
    freed_at: DateTime<Utc>,
    issued_at: DateTime<Utc>,
    stats: AccountStats,
}

impl Account {
    fn roll(&mut self, now: DateTime<Utc>) {
        let was_full = self.full();
        self.short.roll(now);
        self.long.roll(now);
        if was_full && !self.full() {
            self.freed_at = now;
        }
        if !self.full() {
            self.told_full = None;
        }
    }

    fn full(&self) -> bool {
        self.short.full() || self.long.full()
    }

    fn available(&self, now: DateTime<Utc>) -> bool {
        !self.full() && now >= self.blocked_until
    }

    fn clearly_available(&self, now: DateTime<Utc>) -> bool {
        let grace = chrono::Duration::seconds(1);
        self.available(now) && now >= self.blocked_until.max(self.freed_at) + grace
    }

    fn free_at(&self) -> DateTime<Utc> {
        [&self.short, &self.long]
            .into_iter()
            .filter(|w| w.full())
            .map(|w| w.resets)
            .max()
            .unwrap_or(self.short.resets)
    }

    fn limits(&self) -> Limits {
        Limits {
            short_pct: self.short.pct(),
            short_resets: self.short.resets,
            short_full: self.short.full(),
            long_pct: self.long.pct(),
            long_resets: self.long.resets,
            long_full: self.long.full(),
            short_minutes: self.short.period.num_minutes().max(1),
            long_minutes: self.long.period.num_minutes().max(1),
        }
    }
}

#[derive(Clone, Copy)]
struct Limits {
    short_pct: f64,
    short_resets: DateTime<Utc>,
    short_full: bool,
    long_pct: f64,
    long_resets: DateTime<Utc>,
    long_full: bool,
    short_minutes: i64,
    long_minutes: i64,
}

enum Refusal {
    Quota(Limits, DateTime<Utc>),
    Throttle(Limits),
    Error,
    PreviousNotFound,
}

struct Turn {
    id: u64,
    name: String,
    limits: Limits,
    duration: Duration,
    drop: bool,
    busy: Busy,
}

struct Busy {
    fake: Arc<Fake>,
    name: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(a) = self.fake.accounts.lock().unwrap().get_mut(&self.name) {
            a.in_flight -= 1;
        }
    }
}

struct Fake {
    args: Args,
    accounts: Mutex<BTreeMap<String, Account>>,
    owners: Mutex<HashMap<String, String>>,
    sessions: Mutex<HashMap<String, String>>,
    turns: AtomicU64,
    problems: Mutex<BTreeMap<String, (u64, Vec<String>)>>,
    hub: std::sync::OnceLock<String>,
}

impl Fake {
    fn new(args: &Args) -> Arc<Self> {
        let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).expect("valid time");
        let mut rng = rand::rng();
        let mut accounts = BTreeMap::new();
        let total = args.claude_accounts + args.codex_accounts;
        let named = (0..args.claude_accounts)
            .map(|i| (format!("claude-{i:02}"), Provider::Claude))
            .chain((0..args.codex_accounts).map(|i| (format!("codex-{i:02}"), Provider::Codex)));
        for (i, (name, provider)) in named.enumerate() {
            let short_period = chrono::Duration::seconds(args.short_secs);
            let long_period = chrono::Duration::seconds(args.long_secs);
            let spread = |period: chrono::Duration| {
                chrono::Duration::seconds(period.num_seconds() * (i as i64 + 1) / total as i64)
            };
            let jitter = |rng: &mut rand::rngs::ThreadRng| rng.random_range(0.5..1.5);
            let account = Account {
                provider,
                generation: 1,
                revoked: false,
                short: Window {
                    used: 0.0,
                    cap: (args.short_cap * jitter(&mut rng)).round().max(1.0),
                    period: short_period,
                    resets: now + spread(short_period),
                },
                long: Window {
                    used: 0.0,
                    cap: (args.long_cap * jitter(&mut rng)).round().max(1.0),
                    period: long_period,
                    resets: now + spread(long_period),
                },
                blocked_until: now,
                in_flight: 0,
                told_full: None,
                freed_at: now,
                issued_at: now,
                stats: AccountStats::default(),
            };
            accounts.insert(name, account);
        }
        Arc::new(Self {
            args: args.clone(),
            accounts: Mutex::new(accounts),
            owners: Mutex::default(),
            sessions: Mutex::default(),
            turns: AtomicU64::new(0),
            problems: Mutex::default(),
            hub: std::sync::OnceLock::new(),
        })
    }

    fn logins(&self) -> Vec<LoginTokens> {
        self.accounts
            .lock()
            .unwrap()
            .iter()
            .map(|(name, a)| LoginTokens {
                provider: a.provider,
                identity: name.clone(),
                email: Some(format!("{name}@example.com")),
                plan: None,
                chatgpt_account_id: (a.provider == Provider::Codex).then(|| format!("acct-{name}")),
                access_token: format!("at-{name}.1"),
                refresh_token: format!("rt-{name}.1"),
                expires_at: Utc::now() + chrono::Duration::seconds(token_life(a.provider)),
                id_token: None,
            })
            .collect()
    }

    fn problem(&self, kind: &str, detail: String) {
        let mut problems = self.problems.lock().unwrap();
        let entry = problems.entry(kind.to_string()).or_default();
        entry.0 += 1;
        if entry.1.len() < 5 {
            entry.1.push(detail);
        }
    }

    fn wants_example(&self, kind: &str) -> bool {
        self.problems.lock().unwrap().get(kind).is_none_or(|p| p.1.len() < 5)
    }

    fn refuse_full(self: &Arc<Self>, name: &str, a: &mut Account, now: DateTime<Utc>) {
        let kind = format!("{} turn sent to an account after it reported full", a.provider);
        a.stats.refused += 1;
        let told = *a.told_full.get_or_insert(now);
        let late = (now - told).num_milliseconds();
        if late <= 500 {
            return;
        }
        let detail = format!(
            "{name} got a turn {late} ms after it first reported full (short {:.0}%, long {:.0}%)",
            a.short.pct(),
            a.long.pct()
        );
        self.problem_with_view(&kind, detail, name);
    }

    fn problem_with_view(self: &Arc<Self>, kind: &str, detail: String, account: &str) {
        let (Some(hub), true) = (self.hub.get().cloned(), self.wants_example(kind)) else {
            return self.problem(kind, detail);
        };
        let (fake, kind, account) = (self.clone(), kind.to_string(), account.to_string());
        tokio::spawn(async move {
            let view = hub_view(&reqwest::Client::new(), &hub, &account).await;
            fake.problem(&kind, format!("{detail}. Hub view of {account}: {view}"));
        });
    }

    fn current_limits(&self, name: &str) -> Option<Limits> {
        let mut accounts = self.accounts.lock().unwrap();
        let a = accounts.get_mut(name)?;
        a.roll(Utc::now());
        Some(a.limits())
    }

    fn any_available(&self, provider: Provider) -> Option<String> {
        let now = Utc::now();
        let mut accounts = self.accounts.lock().unwrap();
        accounts.iter_mut().find_map(|(name, a)| {
            a.roll(now);
            (a.provider == provider && a.clearly_available(now)).then(|| name.clone())
        })
    }

    fn authorize(&self, headers: &HeaderMap, chaos: bool) -> Result<String, &'static str> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer at-"))
            .ok_or("no token")?;
        let (name, generation) = token.rsplit_once('.').ok_or("bad token")?;
        let generation: u64 = generation.parse().map_err(|_| "bad token")?;
        let mut accounts = self.accounts.lock().unwrap();
        let a = accounts.get_mut(name).ok_or("unknown account")?;
        if generation != a.generation {
            a.stats.stale_token += 1;
            return Err("stale token");
        }
        if a.revoked {
            return Err("revoked token");
        }
        let settled = Utc::now() - a.issued_at > chrono::Duration::seconds(5);
        if chaos && settled && rand::rng().random_bool(self.args.revoke_rate) {
            a.revoked = true;
            a.stats.revoked += 1;
            return Err("revoked token");
        }
        Ok(name.to_string())
    }

    fn begin(self: &Arc<Self>, name: &str, session: Option<&str>, previous: Option<&str>) -> Result<Turn, Refusal> {
        let now = Utc::now();
        let mut rng = rand::rng();
        let mut accounts = self.accounts.lock().unwrap();
        for a in accounts.values_mut() {
            a.roll(now);
        }
        let a = accounts.get_mut(name).expect("authorized account");
        let provider = a.provider;
        if a.full() {
            self.refuse_full(name, a, now);
            return Err(Refusal::Quota(a.limits(), a.free_at()));
        }
        if let Some(previous) = previous {
            let owner = self.owners.lock().unwrap().get(previous).cloned();
            if owner.as_deref() != Some(name) {
                let owner_free = owner
                    .as_ref()
                    .and_then(|o| accounts.get(o))
                    .is_some_and(|o| o.clearly_available(now));
                if owner_free {
                    let owner = owner.unwrap_or_default();
                    self.problem_with_view(
                        "continuation sent away from an owner with room",
                        format!("{previous} owned by {owner} went to {name}"),
                        &owner,
                    );
                }
                return Err(Refusal::PreviousNotFound);
            }
        }
        let a = accounts.get_mut(name).expect("authorized account");
        if rng.random_bool(self.args.error_rate) {
            a.stats.errors += 1;
            return Err(Refusal::Error);
        }
        let crowded = self.args.max_concurrent.is_some_and(|max| a.in_flight >= max);
        if crowded || rng.random_bool(self.args.throttle_rate) {
            a.stats.throttled += 1;
            a.blocked_until = now + chrono::Duration::seconds(THROTTLE_SECS);
            return Err(Refusal::Throttle(a.limits()));
        }
        a.short.used += 1.0;
        a.long.used += 1.0;
        a.stats.served += 1;
        a.in_flight += 1;
        a.stats.peak_in_flight = a.stats.peak_in_flight.max(a.in_flight);
        let drop = rng.random_bool(self.args.drop_rate);
        if drop {
            a.stats.dropped += 1;
        }
        if a.full() {
            a.told_full.get_or_insert(now);
        }
        let limits = a.limits();
        if let Some(session) = session {
            let key = format!("{}:{session}", provider.as_str());
            let before = self.sessions.lock().unwrap().insert(key, name.to_string());
            if let Some(before) = before.filter(|b| b != name)
                && let Some(b) = accounts.get(&before).filter(|b| b.clearly_available(now))
            {
                let detail = format!(
                    "{provider} session {session} moved from {before} to {name}; {before} had {} in flight, short {:.0}%, unblocked {} ms ago",
                    b.in_flight,
                    b.short.pct(),
                    (now - b.blocked_until).num_milliseconds()
                );
                self.problem_with_view("session moved off an account with room", detail, &before);
            }
        }
        drop_lock(accounts);
        let id = self.turns.fetch_add(1, Ordering::SeqCst) + 1;
        if provider == Provider::Codex {
            self.owners
                .lock()
                .unwrap()
                .insert(format!("resp_{id}"), name.to_string());
        }
        let (lo, hi) = (
            self.args.turn_min_ms,
            self.args.turn_max_ms.max(self.args.turn_min_ms + 1),
        );
        Ok(Turn {
            id,
            name: name.to_string(),
            limits,
            duration: Duration::from_millis(rng.random_range(lo..hi)),
            drop,
            busy: Busy {
                fake: self.clone(),
                name: name.to_string(),
            },
        })
    }

    fn refresh(&self, provider: Provider, token: &str) -> Result<(String, String), String> {
        let (name, generation) = token
            .strip_prefix("rt-")
            .and_then(|t| t.rsplit_once('.'))
            .ok_or("bad refresh token")?;
        let generation: u64 = generation.parse().map_err(|_| "bad refresh token")?;
        let mut accounts = self.accounts.lock().unwrap();
        let a = accounts.get_mut(name).ok_or("unknown account")?;
        if a.provider != provider {
            return Err("wrong provider".into());
        }
        if generation != a.generation {
            drop_lock(accounts);
            self.problem(
                "refresh token reused",
                format!("{name} sent refresh generation {generation}"),
            );
            return Err("refresh_token_reused".into());
        }
        a.generation += 1;
        a.revoked = false;
        a.issued_at = Utc::now();
        a.stats.refreshes += 1;
        Ok((
            format!("at-{name}.{}", a.generation),
            format!("rt-{name}.{}", a.generation),
        ))
    }
}

fn token_life(provider: Provider) -> i64 {
    match provider {
        Provider::Claude => 8 * 3600,
        Provider::Codex => 10 * 86400,
    }
}

fn drop_lock<T>(guard: T) {
    drop(guard);
}

fn respond(status: StatusCode, content_type: &str, headers: Vec<(&'static str, String)>, body: Body) -> Response {
    let mut response = Response::builder().status(status).header("content-type", content_type);
    for (name, value) in headers {
        response = response.header(name, value);
    }
    response.body(body).expect("valid response")
}

fn json_reply(status: StatusCode, headers: Vec<(&'static str, String)>, body: Value) -> Response {
    respond(status, "application/json", headers, Body::from(body.to_string()))
}

fn paced(frames: Vec<String>, total: Duration, drop: bool, busy: Busy) -> Body {
    let gap = total / frames.len().max(1) as u32;
    let keep = if drop { frames.len() / 2 } else { frames.len() };
    let mut items: Vec<Result<Bytes, std::io::Error>> =
        frames.into_iter().take(keep).map(|f| Ok(Bytes::from(f))).collect();
    if drop {
        items.push(Err(std::io::Error::other("the fake provider cut the stream")));
    }
    let stream = futures_util::stream::unfold(
        (items.into_iter(), busy, true),
        move |(mut items, busy, first)| async move {
            let item = items.next()?;
            if !first {
                tokio::time::sleep(gap).await;
            }
            Some((item, (items, busy, false)))
        },
    );
    Body::from_stream(stream)
}

fn sse(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message")))
        .collect()
}

fn session_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| headers.get(*n).and_then(|v| v.to_str().ok()).map(str::to_string))
}

fn claude_limit_headers(l: &Limits) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (
            "anthropic-ratelimit-unified-5h-utilization",
            (l.short_pct / 100.0).to_string(),
        ),
        (
            "anthropic-ratelimit-unified-5h-reset",
            l.short_resets.timestamp().to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-utilization",
            (l.long_pct / 100.0).to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset",
            l.long_resets.timestamp().to_string(),
        ),
    ];
    if l.short_full || l.long_full {
        headers.push(("anthropic-ratelimit-unified-status", "rejected".into()));
    }
    if l.short_full {
        headers.push(("anthropic-ratelimit-unified-5h-status", "rejected".into()));
    }
    if l.long_full {
        headers.push(("anthropic-ratelimit-unified-7d-status", "rejected".into()));
    }
    headers
}

fn codex_limit_headers(l: &Limits) -> Vec<(&'static str, String)> {
    vec![
        ("x-codex-primary-used-percent", l.short_pct.to_string()),
        ("x-codex-primary-window-minutes", l.short_minutes.to_string()),
        ("x-codex-primary-reset-at", l.short_resets.timestamp().to_string()),
        ("x-codex-secondary-used-percent", l.long_pct.to_string()),
        ("x-codex-secondary-window-minutes", l.long_minutes.to_string()),
        ("x-codex-secondary-reset-at", l.long_resets.timestamp().to_string()),
    ]
}

fn unauthorized(provider: Provider, why: &str) -> Response {
    let body = match provider {
        Provider::Claude => json!({"type": "error", "error": {"type": "authentication_error", "message": why}}),
        Provider::Codex => json!({"error": {"type": "invalid_request_error", "code": "token_expired", "message": why}}),
    };
    json_reply(StatusCode::UNAUTHORIZED, vec![], body)
}

fn codex_error(kind: &str, message: &str, resets: Option<DateTime<Utc>>) -> Value {
    let mut error = json!({"type": kind, "code": kind, "message": message});
    if let Some(r) = resets {
        error["resets_in_seconds"] = json!((r - Utc::now()).num_seconds().max(1));
    }
    json!({ "error": error })
}

async fn claude_messages(State(fake): State<Arc<Fake>>, headers: HeaderMap, body: Bytes) -> Response {
    let name = match fake.authorize(&headers, true) {
        Ok(n) => n,
        Err(why) => return unauthorized(Provider::Claude, why),
    };
    let session = session_header(&headers, &["x-claude-code-session-id"]);
    let turn = match fake.begin(&name, session.as_deref(), None) {
        Ok(t) => t,
        Err(Refusal::Quota(l, _)) => {
            return json_reply(
                StatusCode::TOO_MANY_REQUESTS,
                claude_limit_headers(&l),
                json!({"type": "error", "error": {"type": "rate_limit_error", "message": "This fake account is out of quota."}}),
            );
        }
        Err(Refusal::Throttle(l)) => {
            let mut h = claude_limit_headers(&l);
            h.push(("retry-after", THROTTLE_SECS.to_string()));
            return json_reply(
                StatusCode::TOO_MANY_REQUESTS,
                h,
                json!({"type": "error", "error": {"type": "rate_limit_error", "message": "Slow down."}}),
            );
        }
        Err(Refusal::Error | Refusal::PreviousNotFound) => {
            return json_reply(
                StatusCode::from_u16(529).unwrap(),
                vec![],
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            );
        }
    };
    let asked: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = asked["model"].as_str().unwrap_or("fake").to_string();
    let id = format!("msg_fake_{}", turn.id);
    let text = format!("Account {} answered turn {}.", turn.name, turn.id);
    let mut events = vec![json!({"type": "message_start", "message": {
        "id": id, "type": "message", "role": "assistant", "model": model, "content": [],
        "stop_reason": null, "stop_sequence": null, "usage": {"input_tokens": 1000, "output_tokens": 1}
    }})];
    events.push(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
    for word in text.split(' ') {
        events.push(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": format!("{word} ")}}));
    }
    events.push(json!({"type": "content_block_stop", "index": 0}));
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 200}}));
    events.push(json!({"type": "message_stop"}));
    let headers = claude_limit_headers(&turn.limits);
    respond(
        StatusCode::OK,
        "text/event-stream",
        headers,
        paced(sse(&events), turn.duration, turn.drop, turn.busy),
    )
}

fn codex_events(turn: &Turn) -> Vec<Value> {
    let id = format!("resp_{}", turn.id);
    let l = &turn.limits;
    let mut events = vec![json!({"type": "response.created", "response": {"id": id}})];
    for i in 0..6 {
        events.push(json!({"type": "response.output_text.delta", "item_id": format!("msg_{}", turn.id), "delta": format!("part {i} ")}));
    }
    events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": {
        "type": "message", "id": format!("msg_{}", turn.id), "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": format!("Account {} answered.", turn.name), "annotations": []}]
    }}));
    events.push(json!({"type": "codex.rate_limits", "rate_limits": {
        "primary": {"used_percent": l.short_pct, "window_minutes": l.short_minutes, "reset_at": l.short_resets.timestamp()},
        "secondary": {"used_percent": l.long_pct, "window_minutes": l.long_minutes, "reset_at": l.long_resets.timestamp()}
    }}));
    events.push(json!({"type": "response.completed", "response": {"id": id, "usage": {
        "input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": 200, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 1200
    }}}));
    events
}

async fn codex_responses(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    body: Bytes,
) -> Response {
    let name = match fake.authorize(&headers, true) {
        Ok(n) => n,
        Err(why) => return unauthorized(Provider::Codex, why),
    };
    let session = session_header(&headers, &["session_id", "session-id"]);
    if let Ok(upgrade) = upgrade {
        let limits = {
            let mut accounts = fake.accounts.lock().unwrap();
            let a = accounts.get_mut(&name).expect("authorized account");
            let now = Utc::now();
            a.roll(now);
            if a.full() {
                fake.refuse_full(&name, a, now);
                let body = codex_error(
                    "usage_limit_reached",
                    "This fake account is out of quota.",
                    Some(a.free_at()),
                );
                return json_reply(StatusCode::TOO_MANY_REQUESTS, vec![], body);
            }
            a.limits()
        };
        let mut response = upgrade.on_upgrade(move |socket| codex_socket(fake, name, session, socket));
        for (k, v) in codex_limit_headers(&limits) {
            response.headers_mut().insert(k, v.parse().unwrap());
        }
        return response;
    }
    let asked: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let previous = asked["previous_response_id"].as_str();
    match fake.begin(&name, session.as_deref(), previous) {
        Ok(turn) => respond(
            StatusCode::OK,
            "text/event-stream",
            codex_limit_headers(&turn.limits),
            paced(sse(&codex_events(&turn)), turn.duration, turn.drop, turn.busy),
        ),
        Err(Refusal::Quota(l, free_at)) => json_reply(
            StatusCode::TOO_MANY_REQUESTS,
            codex_limit_headers(&l),
            codex_error(
                "usage_limit_reached",
                "This fake account is out of quota.",
                Some(free_at),
            ),
        ),
        Err(Refusal::Throttle(l)) => {
            let mut h = codex_limit_headers(&l);
            h.push(("retry-after", THROTTLE_SECS.to_string()));
            json_reply(
                StatusCode::TOO_MANY_REQUESTS,
                h,
                codex_error("rate_limit_exceeded", "Slow down.", None),
            )
        }
        Err(Refusal::Error) => json_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            vec![],
            codex_error("server_error", "The fake provider failed.", None),
        ),
        Err(Refusal::PreviousNotFound) => json_reply(
            StatusCode::BAD_REQUEST,
            vec![],
            codex_error("previous_response_not_found", "Previous response not found.", None),
        ),
    }
}

async fn codex_socket(fake: Arc<Fake>, name: String, session: Option<String>, mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else { continue };
        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let previous = frame["previous_response_id"].as_str();
        let turn = match fake.begin(&name, session.as_deref(), previous) {
            Ok(t) => t,
            Err(refusal) => {
                let mut error = match refusal {
                    Refusal::Quota(_, free_at) => codex_error(
                        "usage_limit_reached",
                        "This fake account is out of quota.",
                        Some(free_at),
                    ),
                    Refusal::Throttle(_) => codex_error("rate_limit_exceeded", "Slow down.", None),
                    Refusal::Error => codex_error("server_error", "The fake provider failed.", None),
                    Refusal::PreviousNotFound => {
                        codex_error("previous_response_not_found", "Previous response not found.", None)
                    }
                };
                error["type"] = "error".into();
                let _ = socket.send(Message::Text(error.to_string().into())).await;
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        };
        let events = codex_events(&turn);
        let gap = turn.duration / events.len() as u32;
        let keep = if turn.drop { events.len() / 2 } else { events.len() };
        for (i, mut event) in events.into_iter().take(keep).enumerate() {
            if i > 0 {
                tokio::time::sleep(gap).await;
            }
            if event["type"] == "codex.rate_limits"
                && let Some(l) = fake.current_limits(&name)
            {
                event["rate_limits"] = json!({
                    "primary": {"used_percent": l.short_pct, "window_minutes": l.short_minutes, "reset_at": l.short_resets.timestamp()},
                    "secondary": {"used_percent": l.long_pct, "window_minutes": l.long_minutes, "reset_at": l.long_resets.timestamp()}
                });
            }
            if socket.send(Message::Text(event.to_string().into())).await.is_err() {
                return;
            }
        }
        drop(turn.busy);
        if turn.drop {
            return;
        }
    }
}

async fn token(State(fake): State<Arc<Fake>>, body: Bytes) -> Response {
    tokio::time::sleep(Duration::from_millis(fake.args.refresh_ms)).await;
    let (provider, refresh_token) = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => (Provider::Claude, v["refresh_token"].as_str().unwrap_or("").to_string()),
        Err(_) => {
            let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
            (Provider::Codex, form.get("refresh_token").cloned().unwrap_or_default())
        }
    };
    match fake.refresh(provider, &refresh_token) {
        Ok((access, refresh)) => json_reply(
            StatusCode::OK,
            vec![],
            json!({"access_token": access, "refresh_token": refresh, "expires_in": token_life(provider), "token_type": "Bearer"}),
        ),
        Err(why) => json_reply(
            StatusCode::BAD_REQUEST,
            vec![],
            json!({"error": "invalid_grant", "error_description": why}),
        ),
    }
}

fn polled_limits(fake: &Fake, headers: &HeaderMap) -> Result<Limits, Box<Response>> {
    let name = fake
        .authorize(headers, false)
        .map_err(|why| Box::new(unauthorized(Provider::Claude, why)))?;
    let mut accounts = fake.accounts.lock().unwrap();
    let a = accounts.get_mut(&name).expect("authorized account");
    a.roll(Utc::now());
    Ok(a.limits())
}

async fn claude_usage(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    match polled_limits(&fake, &headers) {
        Ok(l) => json_reply(
            StatusCode::OK,
            vec![],
            json!({
                "five_hour": {"utilization": l.short_pct, "resets_at": l.short_resets.to_rfc3339()},
                "seven_day": {"utilization": l.long_pct, "resets_at": l.long_resets.to_rfc3339()}
            }),
        ),
        Err(r) => *r,
    }
}

async fn codex_usage(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    match polled_limits(&fake, &headers) {
        Ok(l) => json_reply(
            StatusCode::OK,
            vec![],
            json!({"rate_limit": {
                "primary_window": {"used_percent": l.short_pct, "reset_at": l.short_resets.timestamp(), "limit_window_seconds": l.short_minutes * 60},
                "secondary_window": {"used_percent": l.long_pct, "reset_at": l.long_resets.timestamp(), "limit_window_seconds": l.long_minutes * 60}
            }}),
        ),
        Err(r) => *r,
    }
}

async fn start_provider(fake: Arc<Fake>) -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let routes = axum::Router::new()
        .route("/v1/messages", post(claude_messages))
        .route("/v1/oauth/token", post(token))
        .route("/oauth/token", post(token))
        .route("/api/oauth/usage", get(claude_usage))
        .route("/backend-api/wham/usage", get(codex_usage))
        .route("/backend-api/codex/responses", any(codex_responses))
        .with_state(fake);
    tokio::spawn(async move { axum::serve(listener, routes).await });
    Ok(base)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Outcome {
    Ok,
    Limited,
    Throttled,
    ServerError,
    Unauthorized,
    LostContinuation,
    Cut,
    ConnectFailed,
    Other,
}

const OUTCOMES: [(Outcome, &str); 9] = [
    (Outcome::Ok, "ok"),
    (Outcome::Limited, "all used"),
    (Outcome::Throttled, "throttled"),
    (Outcome::ServerError, "5xx"),
    (Outcome::Unauthorized, "401"),
    (Outcome::LostContinuation, "lost prev"),
    (Outcome::Cut, "cut"),
    (Outcome::ConnectFailed, "no conn"),
    (Outcome::Other, "other"),
];

#[derive(Default)]
struct KindStats {
    outcomes: BTreeMap<Outcome, u64>,
    first_byte_ms: Vec<u64>,
    others: Vec<String>,
}

struct Ctx {
    base: String,
    hub: String,
    http: reqwest::Client,
    deadline: Instant,
    fake: Arc<Fake>,
    stats: Mutex<BTreeMap<&'static str, KindStats>>,
    in_flight: AtomicU64,
}

impl Ctx {
    fn record(&self, kind: &'static str, outcome: Outcome, first_byte: Option<Duration>, detail: Option<String>) {
        let mut stats = self.stats.lock().unwrap();
        let s = stats.entry(kind).or_default();
        *s.outcomes.entry(outcome).or_default() += 1;
        if let Some(d) = first_byte {
            s.first_byte_ms.push(d.as_millis() as u64);
        }
        if let Some(detail) = detail.filter(|_| s.others.len() < 5) {
            s.others.push(detail);
        }
    }

    fn total(&self) -> BTreeMap<Outcome, u64> {
        let mut total = BTreeMap::new();
        for s in self.stats.lock().unwrap().values() {
            for (o, n) in &s.outcomes {
                *total.entry(*o).or_default() += n;
            }
        }
        total
    }

    async fn check_limited(&self, kind: &str, provider: Provider, conflux_said: bool) {
        const KIND: &str = "client told every account is used up while one had room";
        let Some(free) = self.fake.any_available(provider) else {
            return;
        };
        let source = if conflux_said { "conflux" } else { "an upstream refusal" };
        let mut detail = format!("{source} refused a {kind} turn while {free} had room");
        if self.fake.wants_example(KIND) {
            detail.push_str(&format!(
                ". Hub view of {free}: {}",
                hub_view(&self.http, &self.hub, &free).await
            ));
        }
        self.fake.problem(KIND, detail);
    }

    async fn pause(&self, outcome: Outcome, retry_after: Option<u64>) {
        let wait = match outcome {
            Outcome::Ok => rand::rng().random_range(0..=self.fake.args.think_ms),
            Outcome::Limited | Outcome::Throttled => retry_after.unwrap_or(2).clamp(1, 10) * 1000,
            _ => 1000,
        };
        let left = self.deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(Duration::from_millis(wait).min(left)).await;
    }
}

async fn hub_view(http: &reqwest::Client, hub: &str, name: &str) -> String {
    let Ok(resp) = http.get(format!("{hub}/conflux/api/status")).send().await else {
        return "unreachable".into();
    };
    let status: Value = resp.json().await.unwrap_or(Value::Null);
    let email = format!("{name}@example.com");
    let Some(a) = status["accounts"]
        .as_array()
        .and_then(|list| list.iter().find(|a| a["email"] == email.as_str()))
    else {
        return "not listed".into();
    };
    let now = Utc::now();
    let secs_until = |v: &Value| {
        v.as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| format!("{}s", (t.with_timezone(&Utc) - now).num_seconds()))
            .unwrap_or("?".into())
    };
    let windows: Vec<String> = a["usage"]["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|w| {
            format!(
                "{} {}% resets in {}",
                w["key"].as_str().unwrap_or("?"),
                w["used_pct"],
                secs_until(&w["resets_at"])
            )
        })
        .collect();
    let cooldowns: Vec<String> = a["cooldowns"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            format!(
                "{} for {} ({})",
                c["scope"].as_str().unwrap_or("?"),
                secs_until(&c["until"]),
                c["reason"].as_str().unwrap_or("?")
            )
        })
        .collect();
    format!("usage [{}], cooldowns [{}]", windows.join(", "), cooldowns.join(", "))
}

fn classify_status(status: StatusCode, body: &str) -> Outcome {
    match status.as_u16() {
        429 if body.contains("usage_limit_reached")
            || body.contains("out of quota")
            || body.contains("conflux: every") =>
        {
            Outcome::Limited
        }
        429 => Outcome::Throttled,
        401 => Outcome::Unauthorized,
        400 if body.contains("previous_response_not_found") => Outcome::LostContinuation,
        500..=599 => Outcome::ServerError,
        _ => Outcome::Other,
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers.get("retry-after")?.to_str().ok()?.parse().ok()
}

struct Sent {
    outcome: Outcome,
    first_byte: Option<Duration>,
    retry_after: Option<u64>,
    body: String,
    detail: Option<String>,
}

async fn send_http(ctx: &Ctx, req: reqwest::RequestBuilder, done_marker: &str) -> Sent {
    let started = Instant::now();
    ctx.in_flight.fetch_add(1, Ordering::SeqCst);
    let sent = send_http_inner(req, done_marker, started).await;
    ctx.in_flight.fetch_sub(1, Ordering::SeqCst);
    sent
}

async fn send_http_inner(req: reqwest::RequestBuilder, done_marker: &str, started: Instant) -> Sent {
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return Sent {
                outcome: Outcome::ConnectFailed,
                first_byte: None,
                retry_after: None,
                body: String::new(),
                detail: Some(e.to_string()),
            };
        }
    };
    let status = resp.status();
    let retry = retry_after(resp.headers());
    let mut first_byte = None;
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    let mut broke = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(c) => {
                first_byte.get_or_insert_with(|| started.elapsed());
                body.extend_from_slice(&c);
            }
            Err(e) => {
                broke = Some(e.to_string());
                break;
            }
        }
    }
    let body = String::from_utf8_lossy(&body).to_string();
    let outcome = if status.is_success() {
        if broke.is_none() && body.contains(done_marker) {
            Outcome::Ok
        } else {
            Outcome::Cut
        }
    } else {
        classify_status(status, &body)
    };
    let detail =
        (outcome == Outcome::Other).then(|| format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()));
    Sent {
        outcome,
        first_byte,
        retry_after: retry,
        body,
        detail,
    }
}

async fn claude_session(ctx: Arc<Ctx>) {
    let session = uuid::Uuid::new_v4().to_string();
    while Instant::now() < ctx.deadline {
        let req = ctx
            .http
            .post(format!("{}/v1/messages", ctx.base))
            .header("user-agent", "claude-cli/2.1.0 (external, cli)")
            .header("x-app", "cli")
            .header("anthropic-version", "2023-06-01")
            .header("x-claude-code-session-id", &session)
            .json(&json!({
                "model": "claude-sonnet-5-5", "stream": true, "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hello"}]
            }));
        let sent = send_http(&ctx, req, "message_stop").await;
        if sent.outcome == Outcome::Limited {
            ctx.check_limited("claude http", Provider::Claude, sent.body.contains("conflux: every"))
                .await;
        }
        ctx.record("claude http", sent.outcome, sent.first_byte, sent.detail);
        ctx.pause(sent.outcome, sent.retry_after).await;
    }
}

fn completed_id(body: &str) -> Option<String> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .find(|v| v["type"] == "response.completed")
        .and_then(|v| v["response"]["id"].as_str().map(str::to_string))
}

async fn codex_http_session(ctx: Arc<Ctx>) {
    let session = uuid::Uuid::new_v4().to_string();
    let mut previous: Option<String> = None;
    while Instant::now() < ctx.deadline {
        let mut body = json!({"model": "gpt-6.1-sol", "stream": true, "input": [{"role": "user", "content": "hello"}]});
        if let Some(p) = &previous {
            body["previous_response_id"] = json!(p);
        }
        let req = ctx
            .http
            .post(format!("{}/backend-api/codex/responses", ctx.base))
            .header("user-agent", "codex_cli_rs/0.99.0")
            .header("originator", "codex_cli_rs")
            .header("session_id", &session)
            .json(&body);
        let sent = send_http(&ctx, req, "response.completed").await;
        match sent.outcome {
            Outcome::Ok => previous = completed_id(&sent.body),
            Outcome::LostContinuation => previous = None,
            Outcome::Limited => {
                ctx.check_limited("codex http", Provider::Codex, sent.body.contains("conflux: every"))
                    .await
            }
            _ => {}
        }
        ctx.record("codex http", sent.outcome, sent.first_byte, sent.detail);
        ctx.pause(sent.outcome, sent.retry_after).await;
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_connect(ctx: &Ctx, session: &str) -> Result<Socket, (Outcome, Option<String>)> {
    let url = format!("{}/backend-api/codex/responses", ctx.base.replacen("http", "ws", 1));
    let mut req = url
        .into_client_request()
        .map_err(|e| (Outcome::Other, Some(e.to_string())))?;
    req.headers_mut().insert("session_id", session.parse().unwrap());
    req.headers_mut().insert("originator", "codex_cli_rs".parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) => {
            let body = String::from_utf8_lossy(resp.body().as_deref().unwrap_or_default()).to_string();
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let outcome = classify_status(status, &body);
            Err((outcome, Some(format!("handshake HTTP {status}: {body}"))))
        }
        Err(e) => Err((Outcome::ConnectFailed, Some(e.to_string()))),
    }
}

async fn ws_turn(ctx: &Ctx, socket: &mut Socket, previous: &Option<String>) -> (Outcome, Option<Duration>, Value) {
    let mut frame =
        json!({"type": "response.create", "model": "gpt-6.1-sol", "input": [{"role": "user", "content": "hello"}]});
    if let Some(p) = previous {
        frame["previous_response_id"] = json!(p);
    }
    let started = Instant::now();
    if socket
        .send(tungstenite::Message::Text(frame.to_string().into()))
        .await
        .is_err()
    {
        return (Outcome::Cut, None, Value::Null);
    }
    let mut first_byte = None;
    loop {
        let left = ctx.deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(30);
        let msg = match tokio::time::timeout(left, socket.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => return (Outcome::Cut, first_byte, Value::Null),
        };
        let tungstenite::Message::Text(text) = msg else {
            if matches!(msg, tungstenite::Message::Close(_)) {
                return (Outcome::Cut, first_byte, Value::Null);
            }
            continue;
        };
        first_byte.get_or_insert_with(|| started.elapsed());
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match v["type"].as_str() {
            Some("response.completed") => return (Outcome::Ok, first_byte, v),
            Some("error") => {
                let outcome = [
                    ("usage_limit_reached", Outcome::Limited),
                    ("previous_response_not_found", Outcome::LostContinuation),
                    ("rate_limit_exceeded", Outcome::Throttled),
                    ("server_error", Outcome::ServerError),
                ]
                .into_iter()
                .find(|(marker, _)| text.contains(marker))
                .map_or(Outcome::Other, |(_, o)| o);
                return (outcome, first_byte, v);
            }
            _ => {}
        }
    }
}

async fn codex_ws_session(ctx: Arc<Ctx>) {
    let session = uuid::Uuid::new_v4().to_string();
    let mut previous: Option<String> = None;
    let mut socket: Option<Socket> = None;
    while Instant::now() < ctx.deadline {
        ctx.in_flight.fetch_add(1, Ordering::SeqCst);
        if socket.is_none() {
            match ws_connect(&ctx, &session).await {
                Ok(s) => socket = Some(s),
                Err((outcome, detail)) => {
                    ctx.in_flight.fetch_sub(1, Ordering::SeqCst);
                    if outcome == Outcome::Limited {
                        ctx.check_limited(
                            "codex ws handshake",
                            Provider::Codex,
                            detail.as_deref().is_some_and(|d| d.contains("conflux: every")),
                        )
                        .await;
                    }
                    let detail = detail.filter(|_| outcome == Outcome::Other || outcome == Outcome::ConnectFailed);
                    ctx.record("codex ws", outcome, None, detail);
                    ctx.pause(outcome, None).await;
                    continue;
                }
            }
        }
        let (outcome, first_byte, last) = ws_turn(&ctx, socket.as_mut().unwrap(), &previous).await;
        ctx.in_flight.fetch_sub(1, Ordering::SeqCst);
        match outcome {
            Outcome::Ok => previous = last["response"]["id"].as_str().map(str::to_string),
            _ => {
                if outcome == Outcome::LostContinuation {
                    previous = None;
                }
                if outcome == Outcome::Limited {
                    let text = last.to_string();
                    ctx.check_limited("codex ws", Provider::Codex, text.contains("conflux: every"))
                        .await;
                }
                if let Some(mut s) = socket.take() {
                    let _ = s.close(None).await;
                }
            }
        }
        let detail = (outcome == Outcome::Other).then(|| last.to_string());
        ctx.record("codex ws", outcome, first_byte, detail);
        let retry = last["error"]["resets_in_seconds"].as_u64();
        ctx.pause(outcome, retry).await;
    }
    if let Some(mut s) = socket {
        let _ = s.close(None).await;
    }
}

#[derive(Default)]
struct HubWatch {
    peak_rss_kb: u64,
    peak_fds: u64,
    cpu_ticks_start: Option<u64>,
    cpu_ticks_end: u64,
    api_ms: Vec<u64>,
    api_failures: u64,
}

fn proc_stats(pid: u32) -> Option<(u64, u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count() as u64;
    Some((rss, ticks, fds))
}

async fn watch(ctx: Arc<Ctx>, hub_pid: Option<u32>, hub_url: String, watch: Arc<Mutex<HubWatch>>) {
    let started = Instant::now();
    let mut last_print = Instant::now();
    let mut last_ok = 0;
    while Instant::now() < ctx.deadline {
        let t = Instant::now();
        let ok = ctx
            .http
            .get(format!("{hub_url}/conflux/api/dashboard"))
            .send()
            .await
            .map(|r| r.status().is_success());
        let ms = t.elapsed().as_millis() as u64;
        {
            let mut w = watch.lock().unwrap();
            match ok {
                Ok(true) => w.api_ms.push(ms),
                _ => w.api_failures += 1,
            }
            if let Some((rss, ticks, fds)) = hub_pid.and_then(proc_stats) {
                w.peak_rss_kb = w.peak_rss_kb.max(rss);
                w.peak_fds = w.peak_fds.max(fds);
                w.cpu_ticks_start.get_or_insert(ticks);
                w.cpu_ticks_end = ticks;
            }
        }
        if last_print.elapsed() >= Duration::from_secs(5) {
            let total = ctx.total();
            let ok = *total.get(&Outcome::Ok).unwrap_or(&0);
            let rate = (ok - last_ok) as f64 / last_print.elapsed().as_secs_f64();
            last_ok = ok;
            last_print = Instant::now();
            let others: Vec<String> = OUTCOMES
                .iter()
                .skip(1)
                .filter_map(|(o, label)| total.get(o).filter(|n| **n > 0).map(|n| format!("{label} {n}")))
                .collect();
            let rss = hub_pid
                .and_then(proc_stats)
                .map(|(rss, _, fds)| format!("hub {} MB, {fds} fds", rss / 1024))
                .unwrap_or_default();
            println!(
                "{:>4}s  {:>6.1} ok/s  ok {ok}  {}  in flight {}  dashboard {ms} ms  {rss}",
                started.elapsed().as_secs(),
                rate,
                others.join("  "),
                ctx.in_flight.load(Ordering::SeqCst),
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn log_summary(dir: &Path) {
    for name in ["dev-hub", "dev-node"] {
        let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.log"))) else {
            continue;
        };
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for line in text.lines().map(strip_ansi) {
            let level = ["ERROR", "WARN", "INFO"]
                .into_iter()
                .find(|l| line.contains(&format!(" {l} ")));
            let Some(level) = level else { continue };
            let after_level = line.split_once(&format!(" {level} ")).map_or(line.as_str(), |(_, m)| m);
            let message = after_level.split_once(": ").map_or(after_level, |(_, m)| m);
            let message: String = message
                .split_whitespace()
                .filter(|w| !w.contains("account=") && !w.contains("until=") && !w.chars().any(|c| c.is_ascii_digit()))
                .collect::<Vec<_>>()
                .join(" ");
            *counts.entry(format!("{level} {message}")).or_default() += 1;
        }
        let mut counts: Vec<_> = counts.into_iter().collect();
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        println!("{name} log");
        for (message, n) in counts.iter().take(12) {
            println!("  {n:>6}  {}", message.chars().take(150).collect::<String>());
        }
    }
}

fn report(ctx: &Ctx, fake: &Fake, watch: &HubWatch, secs: f64) {
    println!();
    println!("Turns seen by the clients");
    print!("  {:<12}", "client");
    for (_, label) in OUTCOMES {
        print!("{label:>10}");
    }
    println!("{:>10}{:>10}", "p50 ms", "p99 ms");
    let stats = ctx.stats.lock().unwrap();
    for (kind, s) in stats.iter() {
        print!("  {kind:<12}");
        for (o, _) in OUTCOMES {
            print!("{:>10}", s.outcomes.get(&o).unwrap_or(&0));
        }
        let mut ms = s.first_byte_ms.clone();
        ms.sort_unstable();
        println!("{:>10}{:>10}", percentile(&ms, 0.5), percentile(&ms, 0.99));
    }
    println!("  (p50 and p99 are time to first byte through conflux)");
    for (kind, s) in stats.iter() {
        for d in &s.others {
            println!("  {kind} other: {d}");
        }
    }

    println!();
    println!("Accounts at the fake provider");
    println!(
        "  {:<10}{:>8}{:>9}{:>8}{:>8}{:>8}{:>8}{:>8}{:>9}{:>7}{:>8}{:>8}",
        "account",
        "served",
        "refused",
        "thrtl",
        "errors",
        "cut",
        "revoke",
        "stale",
        "refresh",
        "peak",
        "short%",
        "long%"
    );
    let now = Utc::now();
    let mut accounts = fake.accounts.lock().unwrap();
    for (name, a) in accounts.iter_mut() {
        a.roll(now);
        let s = &a.stats;
        println!(
            "  {name:<10}{:>8}{:>9}{:>8}{:>8}{:>8}{:>8}{:>8}{:>9}{:>7}{:>8.0}{:>8.0}",
            s.served,
            s.refused,
            s.throttled,
            s.errors,
            s.dropped,
            s.revoked,
            s.stale_token,
            s.refreshes,
            s.peak_in_flight,
            a.short.pct(),
            a.long.pct()
        );
    }
    let served: u64 = accounts.values().map(|a| a.stats.served).sum();
    drop(accounts);

    println!();
    let problems = fake.problems.lock().unwrap();
    if problems.is_empty() {
        println!("Routing problems: none");
    } else {
        println!("Routing problems");
        for (kind, (n, examples)) in problems.iter() {
            println!("  {n:>6}  {kind}");
            for e in examples {
                println!("            e.g. {e}");
            }
        }
    }

    println!();
    let mut api = watch.api_ms.clone();
    api.sort_unstable();
    let cpu = watch
        .cpu_ticks_start
        .map(|s| (watch.cpu_ticks_end - s) as f64 / 100.0 / secs * 100.0)
        .unwrap_or(0.0);
    println!(
        "Hub: {served} turns in {secs:.0}s ({:.1}/s), CPU {cpu:.0}% of one core, peak memory {} MB, peak open files {}",
        served as f64 / secs,
        watch.peak_rss_kb / 1024,
        watch.peak_fds
    );
    println!(
        "Dashboard API: p50 {} ms, p99 {} ms, max {} ms, {} failed",
        percentile(&api, 0.5),
        percentile(&api, 0.99),
        api.last().unwrap_or(&0),
        watch.api_failures
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let conflux = build_conflux()?;
    let layout = Layout::new()?;
    let fake = Fake::new(&args);
    let base = start_provider(fake.clone()).await?;
    let _ = fake.hub.set(format!("http://127.0.0.1:{}", layout.hub_port));
    let ep = Endpoints::all_at(&base);
    add_accounts(&layout.hub_paths, &ep, fake.logins()).await?;
    let mut env = endpoint_env(&ep);
    env.push(("CONFLUX_SSH", "false".into()));
    let logs = layout.dir.path().to_path_buf();
    let (hub, _node) = layout.start(&conflux, &env, Some(&logs)).await?;

    let hub_url = format!("http://127.0.0.1:{}", layout.hub_port);
    let target = if args.direct {
        hub_url.clone()
    } else {
        format!("http://127.0.0.1:{}", layout.node_port)
    };
    println!(
        "Running {} Claude, {} Codex HTTP, and {} Codex WebSocket sessions against {} Claude and {} Codex fake accounts for {}s, through the {}.",
        args.claude_sessions,
        args.codex_sessions,
        args.ws_sessions,
        args.claude_accounts,
        args.codex_accounts,
        args.secs,
        if args.direct { "hub" } else { "node" }
    );
    println!("Dashboard: {hub_url}/");

    let ctx = Arc::new(Ctx {
        base: target,
        hub: hub_url.clone(),
        http: reqwest::Client::builder().pool_max_idle_per_host(256).build()?,
        deadline: Instant::now() + Duration::from_secs(args.secs),
        fake: fake.clone(),
        stats: Mutex::default(),
        in_flight: AtomicU64::new(0),
    });
    let hub_watch = Arc::new(Mutex::new(HubWatch::default()));
    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..args.claude_sessions {
        tasks.spawn(claude_session(ctx.clone()));
    }
    for _ in 0..args.codex_sessions {
        tasks.spawn(codex_http_session(ctx.clone()));
    }
    for _ in 0..args.ws_sessions {
        tasks.spawn(codex_ws_session(ctx.clone()));
    }
    tasks.spawn(watch(ctx.clone(), hub.id(), hub_url, hub_watch.clone()));

    let finished = tokio::select! {
        _ = async { while tasks.join_next().await.is_some() {} } => true,
        _ = stop_requested() => false,
    };
    tasks.abort_all();
    let secs = started.elapsed().as_secs_f64();
    report(&ctx, &fake, &hub_watch.lock().unwrap(), secs);
    println!();
    log_summary(&logs);
    if !finished {
        println!("Stopped early.");
    }
    Ok(())
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::body::{Body, Bytes};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Request, State};
use axum::response::Response;
use axum::routing::{any, get, post};
use chrono::{DateTime, Utc};
use conflux::accounts::{Provider, Store};
use conflux::config::{Config, Endpoints, Paths, Role};
use conflux::oauth::{self, LoginTokens};
use conflux::refresh::Refresher;
use conflux::setup::{self, ClientPaths};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use tokio::process::{Child, Command};

struct Seed {
    name: &'static str,
    provider: Provider,
    short_pct: f64,
    long_pct: f64,
    days_to_long_reset: i64,
}

const SEEDS: [Seed; 4] = [
    Seed {
        name: "dev-a",
        provider: Provider::Claude,
        short_pct: 20.0,
        long_pct: 35.0,
        days_to_long_reset: 1,
    },
    Seed {
        name: "dev-b",
        provider: Provider::Claude,
        short_pct: 5.0,
        long_pct: 60.0,
        days_to_long_reset: 4,
    },
    Seed {
        name: "dev-c",
        provider: Provider::Codex,
        short_pct: 10.0,
        long_pct: 45.0,
        days_to_long_reset: 2,
    },
    Seed {
        name: "dev-d",
        provider: Provider::Codex,
        short_pct: 30.0,
        long_pct: 15.0,
        days_to_long_reset: 5,
    },
];

const SHORT_WINDOW_SECS: i64 = 5 * 3600;
const LONG_WINDOW_SECS: i64 = 7 * 86400;

#[derive(Clone)]
struct Quota {
    short_pct: f64,
    long_pct: f64,
    short_resets: DateTime<Utc>,
    long_resets: DateTime<Utc>,
    exhausted: bool,
}

impl Quota {
    fn short_used(&self) -> f64 {
        if self.exhausted { 100.0 } else { self.short_pct }
    }
}

struct Pretend {
    quotas: Mutex<HashMap<String, Quota>>,
    turns: AtomicU64,
}

impl Pretend {
    fn new() -> Arc<Self> {
        let now = Utc::now();
        let quotas = SEEDS
            .iter()
            .map(|s| {
                let quota = Quota {
                    short_pct: s.short_pct,
                    long_pct: s.long_pct,
                    short_resets: now + chrono::Duration::hours(3),
                    long_resets: now + chrono::Duration::days(s.days_to_long_reset),
                    exhausted: false,
                };
                (s.name.to_string(), quota)
            })
            .collect();
        Arc::new(Self {
            quotas: Mutex::new(quotas),
            turns: AtomicU64::new(0),
        })
    }

    fn caller(&self, headers: &HeaderMap) -> Option<(String, Quota)> {
        let name = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer at-")?
            .to_string();
        let quota = self.quotas.lock().unwrap().get(&name)?.clone();
        Some((name, quota))
    }

    fn next_turn(&self) -> u64 {
        self.turns.fetch_add(1, Ordering::SeqCst) + 1
    }
}

fn respond(status: StatusCode, content_type: &str, headers: Vec<(&str, String)>, body: String) -> Response {
    let mut response = Response::builder().status(status).header("content-type", content_type);
    for (name, value) in headers {
        response = response.header(name, value);
    }
    response.body(Body::from(body)).expect("valid response")
}

fn json_reply(status: StatusCode, headers: Vec<(&str, String)>, body: Value) -> Response {
    respond(status, "application/json", headers, body.to_string())
}

fn event_stream(headers: Vec<(&str, String)>, events: &[Value]) -> Response {
    let body = events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message")))
        .collect();
    respond(StatusCode::OK, "text/event-stream", headers, body)
}

fn unknown_account() -> Response {
    json_reply(
        StatusCode::UNAUTHORIZED,
        vec![],
        json!({"type": "error", "error": {"type": "authentication_error", "message": "the dev provider does not know this token"}}),
    )
}

fn reply_text(name: &str) -> String {
    format!("Hello from the conflux dev provider. Account {name} answered.")
}

fn claude_limit_headers(q: &Quota) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (
            "anthropic-ratelimit-unified-5h-utilization",
            (q.short_used() / 100.0).to_string(),
        ),
        (
            "anthropic-ratelimit-unified-5h-reset",
            q.short_resets.timestamp().to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-utilization",
            (q.long_pct / 100.0).to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-reset",
            q.long_resets.timestamp().to_string(),
        ),
    ];
    if q.exhausted {
        headers.push(("anthropic-ratelimit-unified-status", "rejected".into()));
        headers.push(("anthropic-ratelimit-unified-5h-status", "rejected".into()));
    }
    headers
}

async fn claude_messages(State(pretend): State<Arc<Pretend>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some((name, quota)) = pretend.caller(&headers) else {
        return unknown_account();
    };
    let limits = claude_limit_headers(&quota);
    if quota.exhausted {
        println!("provider: {name} refused a Claude request, out of quota");
        return json_reply(
            StatusCode::TOO_MANY_REQUESTS,
            limits,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "This dev account is out of quota."}}),
        );
    }
    println!("provider: {name} answered a Claude request");
    let asked: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = asked["model"].as_str().unwrap_or("dev");
    let id = format!("msg_dev_{}", pretend.next_turn());
    let text = reply_text(&name);
    if asked["stream"] != true {
        return json_reply(
            StatusCode::OK,
            limits,
            json!({
                "id": id, "type": "message", "role": "assistant", "model": model,
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn", "stop_sequence": null,
                "usage": {"input_tokens": 1, "output_tokens": 12}
            }),
        );
    }
    event_stream(
        limits,
        &[
            json!({"type": "message_start", "message": {
                "id": id, "type": "message", "role": "assistant", "model": model, "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 12}}),
            json!({"type": "message_stop"}),
        ],
    )
}

async fn claude_usage(State(pretend): State<Arc<Pretend>>, headers: HeaderMap) -> Response {
    let Some((_, quota)) = pretend.caller(&headers) else {
        return unknown_account();
    };
    json_reply(
        StatusCode::OK,
        vec![],
        json!({
            "five_hour": {"utilization": quota.short_used(), "resets_at": quota.short_resets.to_rfc3339()},
            "seven_day": {"utilization": quota.long_pct, "resets_at": quota.long_resets.to_rfc3339()}
        }),
    )
}

async fn codex_usage(State(pretend): State<Arc<Pretend>>, headers: HeaderMap) -> Response {
    let Some((_, quota)) = pretend.caller(&headers) else {
        return unknown_account();
    };
    json_reply(
        StatusCode::OK,
        vec![],
        json!({"rate_limit": {
            "primary_window": {
                "used_percent": quota.short_used(),
                "reset_at": quota.short_resets.timestamp(),
                "limit_window_seconds": SHORT_WINDOW_SECS
            },
            "secondary_window": {
                "used_percent": quota.long_pct,
                "reset_at": quota.long_resets.timestamp(),
                "limit_window_seconds": LONG_WINDOW_SECS
            }
        }}),
    )
}

fn codex_events(name: &str, turn: u64, q: &Quota) -> Vec<Value> {
    let id = format!("resp_dev_{turn}");
    vec![
        json!({"type": "response.created", "response": {"id": id}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "message", "id": format!("msg_dev_{turn}"), "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": reply_text(name), "annotations": []}]
        }}),
        json!({"type": "codex.rate_limits", "rate_limits": {
            "primary": {
                "used_percent": q.short_used(),
                "window_minutes": SHORT_WINDOW_SECS / 60,
                "reset_at": q.short_resets.timestamp()
            },
            "secondary": {
                "used_percent": q.long_pct,
                "window_minutes": LONG_WINDOW_SECS / 60,
                "reset_at": q.long_resets.timestamp()
            }
        }}),
        json!({"type": "response.completed", "response": {"id": id, "usage": {
            "input_tokens": 1, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 12, "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 13
        }}}),
    ]
}

fn codex_out_of_quota(q: &Quota) -> Value {
    json!({"error": {
        "type": "usage_limit_reached",
        "message": "This dev account is out of quota.",
        "resets_in_seconds": (q.short_resets - Utc::now()).num_seconds().max(60)
    }})
}

async fn codex_socket(pretend: Arc<Pretend>, name: String, mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(_) = message else { continue };
        let Some(quota) = pretend.quotas.lock().unwrap().get(&name).cloned() else {
            return;
        };
        if quota.exhausted {
            println!("provider: {name} refused a Codex turn over WebSocket, out of quota");
            let mut refusal = codex_out_of_quota(&quota);
            refusal["type"] = "error".into();
            let _ = socket.send(Message::Text(refusal.to_string().into())).await;
            return;
        }
        println!("provider: {name} answered a Codex turn over WebSocket");
        for event in codex_events(&name, pretend.next_turn(), &quota) {
            if socket.send(Message::Text(event.to_string().into())).await.is_err() {
                return;
            }
        }
    }
}

async fn codex_responses(
    State(pretend): State<Arc<Pretend>>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let Some((name, quota)) = pretend.caller(&headers) else {
        return unknown_account();
    };
    if quota.exhausted {
        println!("provider: {name} refused a Codex request, out of quota");
        return json_reply(StatusCode::TOO_MANY_REQUESTS, vec![], codex_out_of_quota(&quota));
    }
    match upgrade {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| codex_socket(pretend, name, socket)),
        Err(_) => {
            println!("provider: {name} answered a Codex turn over HTTP");
            event_stream(vec![], &codex_events(&name, pretend.next_turn(), &quota))
        }
    }
}

fn set_exhausted(pretend: &Pretend, name: &str, exhausted: bool) -> Response {
    let known = pretend
        .quotas
        .lock()
        .unwrap()
        .get_mut(name)
        .map(|q| q.exhausted = exhausted)
        .is_some();
    let (status, text) = match (known, exhausted) {
        (false, _) => (
            StatusCode::NOT_FOUND,
            format!("There is no dev account named {name}.\n"),
        ),
        (true, true) => (
            StatusCode::OK,
            format!("{name} is out of quota until you restore it.\n"),
        ),
        (true, false) => (StatusCode::OK, format!("{name} has quota again.\n")),
    };
    respond(status, "text/plain", vec![], text)
}

async fn exhaust(State(pretend): State<Arc<Pretend>>, UrlPath(name): UrlPath<String>) -> Response {
    set_exhausted(&pretend, &name, true)
}

async fn restore(State(pretend): State<Arc<Pretend>>, UrlPath(name): UrlPath<String>) -> Response {
    set_exhausted(&pretend, &name, false)
}

async fn no_reply(req: Request) -> Response {
    println!("provider: no reply for {} {}", req.method(), req.uri().path());
    json_reply(
        StatusCode::NOT_FOUND,
        vec![],
        json!({"type": "error", "error": {"type": "not_found_error", "message": "the dev provider has no reply for this path"}}),
    )
}

async fn start_provider() -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let routes = axum::Router::new()
        .route("/v1/messages", post(claude_messages))
        .route("/api/oauth/usage", get(claude_usage))
        .route("/backend-api/wham/usage", get(codex_usage))
        .route("/backend-api/codex/responses", any(codex_responses))
        .route("/dev/exhaust/{name}", post(exhaust))
        .route("/dev/restore/{name}", post(restore))
        .fallback(no_reply)
        .with_state(Pretend::new());
    tokio::spawn(async move { axum::serve(listener, routes).await });
    Ok(base)
}

fn build_conflux() -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = std::process::Command::new(cargo);
    build
        .args(["build", "--bin", "conflux"])
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    if !build.status().context("run cargo")?.success() {
        bail!("could not build conflux");
    }
    let exe = std::env::current_exe()?;
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .context("find the target directory")?;
    Ok(profile_dir.join(format!("conflux{}", std::env::consts::EXE_SUFFIX)))
}

fn build_mac_app(conflux: &Path) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("--app needs macOS");
    }
    let out = conflux.parent().context("find the target directory")?.join("dev-app");
    let built = std::process::Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("macos/build-app.sh"))
        .arg(conflux)
        .arg(&out)
        .env("CONFLUX_BUNDLE_ID", "dev.conflux.app.dev")
        .status()
        .context("run macos/build-app.sh")?;
    if !built.success() {
        bail!("could not build the Mac app");
    }
    Ok(out.join("Conflux.app/Contents/MacOS/Conflux"))
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn endpoint_env(ep: &Endpoints) -> Vec<(&'static str, String)> {
    vec![
        ("CONFLUX_ANTHROPIC_API", ep.anthropic_api.clone()),
        ("CONFLUX_CLAUDE_AUTHORIZE", ep.claude_authorize.clone()),
        ("CONFLUX_CLAUDE_TOKEN", ep.claude_token.clone()),
        ("CONFLUX_CODEX_API", ep.codex_api.clone()),
        ("CONFLUX_CHATGPT_BACKEND", ep.chatgpt_backend.clone()),
        ("CONFLUX_OPENAI_AUTH", ep.openai_auth.clone()),
        ("CONFLUX_RELEASES", ep.releases.clone()),
    ]
}

async fn add_accounts(paths: &Paths, ep: &Endpoints) -> Result<()> {
    let store = Arc::new(Store::load(&paths.accounts_dir())?);
    let refresher = Refresher::new(store, ep.clone(), oauth::control_client(), true);
    for seed in &SEEDS {
        let name = seed.name;
        refresher
            .install_login(LoginTokens {
                provider: seed.provider,
                identity: name.into(),
                email: Some(format!("{name}@example.com")),
                plan: None,
                chatgpt_account_id: (seed.provider == Provider::Codex).then(|| format!("acct-{name}")),
                access_token: format!("at-{name}"),
                refresh_token: format!("rt-{name}"),
                expires_at: Utc::now() + chrono::Duration::days(30),
                id_token: None,
            })
            .await?;
    }
    Ok(())
}

struct Machine<'a> {
    conflux: &'a Path,
    home: &'a Path,
    name: &'a str,
    clients: &'a Path,
}

impl Machine<'_> {
    fn start(&self, env: &[(&'static str, String)]) -> Result<Child> {
        Command::new(self.conflux)
            .arg("worker")
            .env("CONFLUX_HOME", self.home)
            .env("CONFLUX_HOSTNAME", self.name)
            .env("CLAUDE_CONFIG_DIR", self.clients.join("claude"))
            .env("CODEX_HOME", self.clients.join("codex"))
            .env_remove("CONFLUX_HANDOFF")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {}", self.name))
    }
}

async fn wait_until_up(child: &mut Child, port: u16, name: &str) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/conflux/api/status");
    let http = reqwest::Client::new();
    for _ in 0..300 {
        if let Some(status) = child.try_wait()? {
            bail!("{name} stopped before it was ready ({status})");
        }
        let up = http.get(&url).send().await.is_ok_and(|r| r.status().is_success());
        if up {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("{name} did not start in time")
}

async fn stop_requested() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("watch for SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let real = std::env::args().any(|a| a == "--real");
    let conflux = build_conflux()?;
    let mac_app = std::env::args()
        .any(|a| a == "--app")
        .then(|| build_mac_app(&conflux))
        .transpose()?;
    let dir = tempfile::tempdir()?;
    let hub_home = dir.path().join("hub");
    let node_home = dir.path().join("node");
    let hub_paths = Paths::under(&hub_home);
    let node_paths = Paths::under(&node_home);
    let hub_port = free_port()?;
    let node_port = free_port()?;

    Config {
        role: Role::Hub,
        port: hub_port,
        ..Config::default()
    }
    .save(&hub_paths)?;
    Config {
        role: Role::Node,
        hub: Some("127.0.0.1".into()),
        port: node_port,
        ..Config::default()
    }
    .save(&node_paths)?;
    let clients = ClientPaths {
        claude_settings: dir.path().join("claude").join("settings.json"),
        codex_config: dir.path().join("codex").join("config.toml"),
    };
    setup::apply(&clients, &node_paths.config_dir, node_port)?;

    let provider = if real { None } else { Some(start_provider().await?) };
    let mut env = Vec::new();
    if let Some(base) = &provider {
        let ep = Endpoints::all_at(base);
        add_accounts(&hub_paths, &ep).await?;
        env = endpoint_env(&ep);
    }

    env.push((
        "CONFLUX_SSH",
        std::env::var("CONFLUX_SSH").unwrap_or_else(|_| "false".into()),
    ));
    let mut hub = Machine {
        conflux: &conflux,
        home: &hub_home,
        name: "dev-hub",
        clients: dir.path(),
    }
    .start(&env)?;
    wait_until_up(&mut hub, hub_port, "the dev hub").await?;
    env.push(("CONFLUX_HUB_PORT", hub_port.to_string()));
    let mut node = Machine {
        conflux: &conflux,
        home: &node_home,
        name: "dev-node",
        clients: dir.path(),
    }
    .start(&env)?;
    wait_until_up(&mut node, node_port, "the dev node").await?;
    let menu_bar = mac_app
        .as_ref()
        .map(|app| {
            Command::new(app)
                .env("CONFLUX_HOME", &node_home)
                .env("CLAUDE_CONFIG_DIR", dir.path().join("claude"))
                .env("CODEX_HOME", dir.path().join("codex"))
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("start the Mac app")
        })
        .transpose()?;

    let conflux = conflux.display();
    let hub_home = hub_home.display();
    let node_home = node_home.display();
    let claude_dir = dir.path().join("claude");
    let codex_dir = dir.path().join("codex");
    println!();
    println!("The conflux dev copy is running. It does not touch the real install.");
    println!();
    println!("  Dashboard  http://127.0.0.1:{hub_port}/");
    println!("  Hub        port {hub_port}");
    println!("  Node       port {node_port}, forwards to the hub");
    println!("  Files      {}", dir.path().display());
    println!();
    println!("Run Claude Code through the node:");
    println!("  CLAUDE_CONFIG_DIR={} claude -p \"say hi\"", claude_dir.display());
    println!("Run Codex through the node:");
    println!(
        "  CODEX_HOME={} codex exec --skip-git-repo-check \"say hi\"",
        codex_dir.display()
    );
    println!("Run the CLI against the hub:");
    println!("  CONFLUX_HOME={hub_home} {conflux} status");
    match &provider {
        Some(base) => {
            println!("Use up an account's quota, then give it back:");
            println!("  curl -X POST {base}/dev/exhaust/dev-a");
            println!("  curl -X POST {base}/dev/restore/dev-a");
        }
        None => {
            println!("Add a real account, and approve it in a browser:");
            println!("  CONFLUX_HOME={hub_home} {conflux} login claude");
            println!("  CONFLUX_HOME={hub_home} {conflux} login codex");
        }
    }
    if let Some(app) = &mac_app {
        println!("The Mac app is in the menu bar and shows the node. Draw its menu to a picture:");
        println!("  CONFLUX_HOME={node_home} {} --snapshot menu.png", app.display());
    }
    println!();
    println!(
        "Press Ctrl-C or run `kill {}` to stop. The files are removed on exit.",
        std::process::id()
    );
    println!();

    let stopped = tokio::select! {
        _ = stop_requested() => None,
        status = hub.wait() => Some(format!("the dev hub stopped ({})", status?)),
        status = node.wait() => Some(format!("the dev node stopped ({})", status?)),
    };
    if let Some(mut menu_bar) = menu_bar {
        let _ = menu_bar.kill().await;
    }
    let _ = hub.kill().await;
    let _ = node.kill().await;
    match stopped {
        Some(why) => bail!(why),
        None => Ok(()),
    }
}

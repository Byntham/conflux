use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::body::Body;
use axum::extract::ws::{Message as ClientMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

use crate::access::{self, Verdict};
use crate::api::UpdateRequest;
use crate::config::{Config, Endpoints, Paths};
use crate::lifecycle::{InFlight, Lifecycle};
use crate::setup::{self, ClientPaths};
use crate::tailnet::{Caller, TailnetSelf, Tailscale};
use crate::transcript_sync::TranscriptSync;
use crate::update::{self, Updater};
use crate::ws::TurnTracker;
use crate::{hubmove, link, proxy, tailnet, ws};

pub const NODE_HEADER: &str = "x-conflux-node";
const REPORT_EVERY: Duration = Duration::from_secs(20);
const HUB_SILENCE_LIMIT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, Serialize)]
pub struct HubLink {
    pub connected: bool,
    pub last_error: Option<String>,
}

pub struct Node {
    pub port: u16,
    hub_port: u16,
    hub: RwLock<String>,
    paths: Paths,
    http: reqwest::Client,
    hostname: String,
    has_battery: bool,
    allowed_hosts: Vec<String>,
    hub_link: Mutex<HubLink>,
    report_requested: Notify,
    clients: ClientPaths,
    tailscale: Tailscale,
    guest: AtomicBool,
    access: Mutex<Value>,
    pub lifecycle: Arc<Lifecycle>,
    pub updater: Arc<Updater>,
}

const HOP_BY_HOP: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "origin",
    "content-length",
];
const CLIENT_CREDENTIALS: &[&str] = &["authorization", "x-api-key", "cookie"];

fn forwarded_headers(path: &str, src: &HeaderMap, also_skip: &[&str]) -> HeaderMap {
    if crate::access::is_proxy_path(path) {
        copy_headers(src, &[HOP_BY_HOP, CLIENT_CREDENTIALS, also_skip])
    } else {
        copy_headers(src, &[HOP_BY_HOP, also_skip])
    }
}

const HANDSHAKE: &[&str] = &[
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-accept",
];

fn copy_headers(src: &HeaderMap, skip: &[&[&str]]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        let name = k.as_str();
        if skip.iter().any(|list| list.contains(&name)) || name.starts_with("proxy-") {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    out
}

pub fn hub_port(port: u16) -> u16 {
    std::env::var("CONFLUX_HUB_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(port)
}

impl Node {
    pub fn new(hub: String, port: u16, paths: Paths, clients: ClientPaths) -> Arc<Self> {
        Self::named(
            tailnet::hostname(),
            hub,
            hub_port(port),
            port,
            paths,
            clients,
            tailnet::system_tailscale(),
        )
    }

    pub fn named(
        hostname: String,
        hub: String,
        hub_port: u16,
        port: u16,
        paths: Paths,
        clients: ClientPaths,
        tailscale: Tailscale,
    ) -> Arc<Self> {
        Arc::new(Self {
            port,
            hub_port,
            hub: RwLock::new(hub),
            paths,
            http: crate::app::proxy_client(),
            hostname,
            has_battery: hubmove::is_laptop(),
            allowed_hosts: access::local_hosts(port),
            hub_link: Mutex::default(),
            report_requested: Notify::new(),
            clients,
            tailscale,
            guest: AtomicBool::new(false),
            access: Mutex::default(),
            lifecycle: Arc::default(),
            updater: Updater::new(&Endpoints::from_env().releases),
        })
    }

    pub fn hub(&self) -> String {
        self.hub.read().unwrap().clone()
    }

    fn hub_base(&self, scheme: &str) -> String {
        format!("{scheme}://{}:{}", self.hub(), self.hub_port)
    }

    fn node_header(&self) -> HeaderValue {
        HeaderValue::from_str(&self.hostname).unwrap_or(HeaderValue::from_static("unknown"))
    }

    fn hub_down(&self, path: &str, headers: &HeaderMap) -> Response {
        let message = format!(
            "conflux: the hub ({}) is unreachable. Run `conflux off` to use this machine's own logins until it is back.",
            self.hub()
        );
        let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("");
        match proxy::provider_for(path, ua) {
            Some(p) => proxy::provider_error(p, StatusCode::SERVICE_UNAVAILABLE, "api_error", &message),
            None => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": message }))).into_response(),
        }
    }

    pub fn routes(self: &Arc<Self>) -> axum::Router {
        axum::Router::new()
            .route("/", get(dashboard_redirect))
            .route("/conflux/node", get(node_status))
            .route("/conflux/node/report", post(report_now))
            .route("/conflux/node/ask", post(ask_again))
            .route("/conflux/api/update", post(update))
            .fallback(forward)
            .layer(axum::middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }

    fn report(&self) -> Message {
        let cfg = Config::load(&self.paths).unwrap_or_default();
        let report = json!({
            "hostname": self.hostname,
            "version": update::CURRENT,
            "transcript_sync": !self.is_guest() && cfg.transcript_sync,
            "ask": cfg.ask,
            "routing": setup::routing(&self.paths.config_dir),
            "update": self.updater.status(),
            "has_battery": self.has_battery,
        });
        Message::Text(report.to_string().into())
    }

    pub async fn follow_redirect(&self, new_hub: &str) -> Result<bool> {
        if new_hub == self.hub() {
            return Ok(false);
        }
        let tailscale = self.tailscale.clone();
        let me = tokio::task::spawn_blocking(move || tailnet::status(&tailscale).map(|s| TailnetSelf::from_status(&s)))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let named_me = me.names().contains(&new_hub.trim_end_matches('.').to_lowercase());
        let address = new_hub.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok();
        if named_me || address.is_some_and(|a| me.ips.contains(&a)) {
            tracing::warn!(new_hub, "ignoring a hub redirect to this machine");
            return Ok(false);
        }
        if !tailnet::resolves_to_tailnet(new_hub, self.hub_port).await {
            tracing::warn!(new_hub, "ignoring a hub redirect that is not a tailnet address");
            return Ok(false);
        }
        let mut cfg = Config::load(&self.paths)?;
        cfg.hub = Some(new_hub.to_string());
        cfg.save(&self.paths)?;
        *self.hub.write().unwrap() = new_hub.to_string();
        tracing::info!(new_hub, "following the hub to its new address");
        Ok(true)
    }

    fn route_while_approved(&self, approved: bool) -> Result<()> {
        let mut cfg = Config::load(&self.paths)?;
        let change = if approved {
            cfg.route_once_approved
        } else {
            setup::routing(&self.paths.config_dir)
        };
        if change {
            setup::set_routing(&self.clients, &self.paths.config_dir, self.port, approved)?;
            cfg.route_once_approved = !approved;
            cfg.save(&self.paths)?;
        }
        Ok(())
    }

    pub fn is_guest(&self) -> bool {
        self.guest.load(Ordering::SeqCst)
    }

    async fn obey(self: &Arc<Self>, command: &str) -> bool {
        let command: Value = serde_json::from_str(command).unwrap_or(Value::Null);
        if self.is_guest() && matches!(command["type"].as_str(), Some("routing" | "update")) {
            tracing::warn!(
                "ignoring a {} command, because this machine is a guest of the hub",
                command["type"]
            );
            return false;
        }
        match (
            command["type"].as_str(),
            command["hub"].as_str(),
            command["version"].as_str(),
        ) {
            (Some("routing"), _, _) => {
                if let Some(on) = command["on"].as_bool()
                    && let Err(e) = setup::set_routing(&self.clients, &self.paths.config_dir, self.port, on)
                {
                    tracing::warn!("could not turn routing {}: {e:#}", if on { "on" } else { "off" });
                }
                false
            }
            (Some("providers"), _, _) if self.is_guest() && self.access.lock().unwrap()["state"] != "approved" => false,
            (Some("providers"), _, _) => {
                let providers = command["providers"].as_array().map(|listed| {
                    listed
                        .iter()
                        .filter_map(|p| serde_json::from_value(p.clone()).ok())
                        .collect()
                });
                if let Some(providers) = providers
                    && let Err(e) = setup::set_providers(&self.clients, &self.paths.config_dir, self.port, providers)
                {
                    tracing::warn!("could not route the hub's providers: {e:#}");
                }
                false
            }
            (Some("access"), _, _) => {
                if let Err(e) = self.route_while_approved(command["state"] == "approved") {
                    tracing::warn!("could not change routing after the hub's decision: {e:#}");
                }
                *self.access.lock().unwrap() = command;
                false
            }
            (Some("hub"), Some(hub), _) => match self.follow_redirect(hub).await {
                Ok(moved) => moved,
                Err(e) => {
                    tracing::warn!("could not follow the hub to {hub}: {e:#}");
                    false
                }
            },
            (Some("become_hub"), _, _) => self.become_hub(command.clone()).await,
            (Some("update"), _, Some(version)) => {
                let update = self
                    .updater
                    .clone()
                    .run(version.to_string(), self.lifecycle.clone(), self.clone());
                tokio::spawn(update);
                false
            }
            _ => false,
        }
    }

    async fn become_hub(self: &Arc<Self>, command: Value) -> bool {
        let Ok(bundle) = serde_json::from_value::<hubmove::Bundle>(command) else {
            return false;
        };
        let updating = self.updater.status().is_some_and(|s| s.in_progress());
        let imported = if self.is_guest() {
            Err(anyhow::anyhow!("the hub belongs to another Tailscale user"))
        } else if updating || self.lifecycle.is_draining() {
            Err(anyhow::anyhow!("this machine is updating"))
        } else if self.paths.has_accounts() {
            Err(anyhow::anyhow!("this machine already holds accounts"))
        } else {
            hubmove::import_bundle(&self.paths, &bundle, &self.hub(), self.hub_port)
        };
        match imported {
            Ok((dir, pending)) => self.finish_move(dir, pending).await,
            Err(e) => {
                tracing::warn!("refused to become the hub: {e:#}");
                let _ = self
                    .http
                    .post(format!("{}/conflux/move/abort", self.hub_base("http")))
                    .json(&json!({ "id": bundle.id }))
                    .timeout(CONNECT_TIMEOUT)
                    .send()
                    .await;
                false
            }
        }
    }

    pub async fn resume_move(self: &Arc<Self>) -> Result<bool> {
        match hubmove::find_pending(&self.paths)? {
            Some((dir, pending)) => Ok(self.finish_move(dir, pending).await),
            None => Ok(false),
        }
    }

    async fn finish_move(self: &Arc<Self>, dir: PathBuf, pending: hubmove::Pending) -> bool {
        let became_hub = match hubmove::commit_move(&self.http, &pending).await {
            Ok(true) => hubmove::become_hub(&self.paths, &dir, &pending),
            Ok(false) => {
                tracing::warn!(old_hub = %pending.old_hub, "the hub move was cancelled");
                if let Err(e) = hubmove::discard_incoming(&dir) {
                    tracing::warn!("could not remove the cancelled move: {e:#}");
                }
                return false;
            }
            Err(e) => {
                tracing::warn!("{e:#}. conflux tries again when it restarts.");
                return false;
            }
        };
        if let Err(e) = became_hub {
            tracing::error!("could not take over the accounts: {e:#}");
            return false;
        }
        tracing::info!(old_hub = %pending.old_hub, "this machine is now the hub");
        hubmove::hand_off_and_drain(&self.lifecycle, self.updater.exe(), &**self).await;
        true
    }

    async fn control_session(self: &Arc<Self>) -> Result<()> {
        let url = format!("{}/conflux/control", self.hub_base("ws"));
        let (mut hub, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
            .await
            .context("the hub did not answer")??;
        let MaybeTlsStream::Plain(socket) = hub.get_ref() else {
            bail!("the hub connection is not plain TCP");
        };
        let address = socket.peer_addr()?.ip();
        let tailscale = self.tailscale.clone();
        let guest = match tokio::task::spawn_blocking(move || tailnet::identify(&tailscale, address)).await? {
            Caller::Unknown => bail!("could not ask Tailscale who runs the hub"),
            caller => !caller.is_owner(),
        };
        self.guest.store(guest, Ordering::SeqCst);
        hub.send(self.report()).await?;
        *self.hub_link.lock().unwrap() = HubLink {
            connected: true,
            last_error: None,
        };
        let mut status = self.updater.watch_status();
        let mut report_due = tokio::time::interval(REPORT_EVERY);
        report_due.reset();
        let quiet = tokio::time::sleep(HUB_SILENCE_LIMIT);
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                () = &mut quiet => bail!("the hub went quiet"),
                _ = report_due.tick() => {
                    hub.send(self.report()).await?;
                    hub.send(Message::Ping(Vec::new().into())).await?;
                }
                changed = status.changed() => {
                    changed?;
                    hub.send(self.report()).await?;
                }
                () = self.report_requested.notified() => hub.send(self.report()).await?,
                frame = hub.next() => match frame {
                    Some(Ok(Message::Text(command))) => {
                        quiet.as_mut().reset(tokio::time::Instant::now() + HUB_SILENCE_LIMIT);
                        if self.obey(command.as_str()).await {
                            return Ok(());
                        }
                        hub.send(self.report()).await?;
                    }
                    Some(Ok(Message::Close(_))) | None => bail!("the hub closed the connection"),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(_)) => quiet.as_mut().reset(tokio::time::Instant::now() + HUB_SILENCE_LIMIT),
                },
            }
        }
    }

    pub async fn run_control(self: Arc<Self>) {
        let mut wait = RECONNECT_MIN;
        while !self.lifecycle.is_draining() {
            let ended = self.control_session().await;
            let was_connected = {
                let mut link = self.hub_link.lock().unwrap();
                if let Err(e) = ended {
                    link.last_error = Some(format!("{e:#}"));
                }
                std::mem::take(&mut link.connected)
            };
            if was_connected {
                wait = RECONNECT_MIN;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RECONNECT_MAX);
        }
    }
}

impl update::Handoff for Node {
    async fn pause(&self) -> (Value, mpsc::UnboundedReceiver<Value>) {
        (Value::Null, mpsc::unbounded_channel().1)
    }

    async fn resume(&self) {}
}

async fn guard(State(node): State<Arc<Node>>, req: Request, next: Next) -> Response {
    match access::check(&node.allowed_hosts, req.method(), req.uri().path(), req.headers()) {
        Verdict::Allow => next.run(req).await,
        Verdict::Deny(status, message) => (status, Json(json!({ "error": message }))).into_response(),
    }
}

async fn dashboard_redirect(State(node): State<Arc<Node>>) -> Redirect {
    Redirect::temporary(&format!("{}/", node.hub_base("http")))
}

async fn node_status(State(node): State<Arc<Node>>) -> Json<Value> {
    Json(json!({
        "role": "node",
        "hostname": node.hostname,
        "hub": node.hub(),
        "port": node.port,
        "version": update::CURRENT,
        "pid": std::process::id(),
        "hub_link": node.hub_link.lock().unwrap().clone(),
        "guest": node.is_guest(),
        "access": node.access.lock().unwrap().clone(),
        "update": node.updater.status(),
    }))
}

async fn report_now(State(node): State<Arc<Node>>) -> StatusCode {
    node.report_requested.notify_one();
    StatusCode::NO_CONTENT
}

async fn ask_again(State(node): State<Arc<Node>>) -> Response {
    let asked = Config::load(&node.paths).and_then(|cfg| {
        Config {
            ask: Some(uuid::Uuid::new_v4().to_string()),
            ..cfg
        }
        .save(&node.paths)
    });
    if let Err(e) = asked {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        )
            .into_response();
    }
    node.report_requested.notify_one();
    StatusCode::NO_CONTENT.into_response()
}

async fn update(State(node): State<Arc<Node>>, req: Request) -> Response {
    if !node.is_guest() {
        return forward(State(node), req).await;
    }
    let Json(asked) = match Json::<UpdateRequest>::from_request(req, &()).await {
        Ok(asked) => asked,
        Err(e) => return e.into_response(),
    };
    let version = match crate::api::latest_release(&node.updater).await {
        Ok(version) => version,
        Err(e) => return e.into_response(),
    };
    let lifecycle = node.lifecycle.clone();
    let updater = node.updater.clone();
    let hostname = node.hostname.clone();
    Json(link::update_this_machine(
        &updater, &lifecycle, node, &hostname, &version, &asked,
    ))
    .into_response()
}

async fn forward(State(node): State<Arc<Node>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or(path.clone());
    let is_ws = parts
        .headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    if is_ws {
        return forward_ws(node, parts, path_and_query).await;
    }
    let mut headers = forwarded_headers(&path, &parts.headers, &[]);
    headers.insert(NODE_HEADER, node.node_header());
    let url = format!("{}{path_and_query}", node.hub_base("http"));
    let sent = node
        .http
        .request(parts.method.clone(), url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("hub request failed: {e}");
            return node.hub_down(&path, &parts.headers);
        }
    };
    let mut out = Response::builder().status(resp.status());
    if let Some(h) = out.headers_mut() {
        *h = copy_headers(resp.headers(), &[HOP_BY_HOP]);
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .expect("valid response")
}

async fn forward_ws(node: Arc<Node>, mut parts: http::request::Parts, path_and_query: String) -> Response {
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let url = format!("{}{path_and_query}", node.hub_base("ws"));
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    for (k, v) in forwarded_headers(parts.uri.path(), &parts.headers, HANDSHAKE) {
        if let Some(k) = k {
            req.headers_mut().insert(k, v);
        }
    }
    req.headers_mut().insert(NODE_HEADER, node.node_header());
    let in_flight = node.lifecycle.in_flight();
    let drain = node.lifecycle.watch_drain();
    match tokio_tungstenite::connect_async(req).await {
        Ok((upstream, _)) => upgrade.on_upgrade(move |client| pipe(client, upstream, drain, in_flight)),
        Err(tungstenite::Error::Http(resp)) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            (status, resp.body().clone().unwrap_or_default()).into_response()
        }
        Err(e) => {
            tracing::warn!("hub websocket failed: {e}");
            node.hub_down(parts.uri.path(), &parts.headers)
        }
    }
}

async fn pipe(
    mut client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    mut drain: tokio::sync::watch::Receiver<bool>,
    _in_flight: InFlight,
) {
    let (mut up_tx, mut up_rx) = upstream.split();
    let mut turns = TurnTracker::default();
    loop {
        tokio::select! {
            biased;
            msg = client.recv() => {
                let Some(Ok(msg)) = msg else {
                    let _ = up_tx.send(tungstenite::Message::Close(None)).await;
                    break;
                };
                turns.client_sent(&msg);
                let closing = matches!(msg, ClientMsg::Close(_));
                if up_tx.send(ws::to_upstream(msg)).await.is_err() || closing {
                    break;
                }
            }
            msg = up_rx.next() => {
                let Some(Ok(msg)) = msg else {
                    let _ = client.send(ClientMsg::Close(None)).await;
                    break;
                };
                if let Message::Text(text) = &msg {
                    turns.upstream_sent(text.as_str());
                }
                let closing = matches!(msg, tungstenite::Message::Close(_));
                let Some(out) = ws::to_client(msg) else { continue };
                if client.send(out).await.is_err() || closing {
                    break;
                }
            }
            () = ws::stop_requested(&mut drain), if turns.idle() => {
                let _ = client.send(ClientMsg::Close(None)).await;
                let _ = up_tx.send(Message::Close(None)).await;
                break;
            }
        }
    }
}

pub async fn serve(cfg: Config, paths: Paths) -> Result<()> {
    let hub = cfg
        .hub
        .clone()
        .context("this machine is a node but has no hub set; run `conflux join <hub>`")?;
    let node = Node::new(hub.clone(), cfg.port, paths, ClientPaths::from_env()?);
    let takeover = node.lifecycle.take_over().await?;
    let exe = update::running_exe()?;
    node.updater.replaces(exe.clone());
    let for_sync = node.clone();
    let sync = TranscriptSync::new(
        node.hostname.clone(),
        node.paths.clone(),
        &node.clients,
        Arc::new(move || {
            let owned = for_sync.hub_link.lock().unwrap().connected && !for_sync.is_guest();
            owned.then(|| for_sync.hub_base("http"))
        }),
    );
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = node.lifecycle.listen(addr).with_context(|| format!("bind {addr}"))?;
    node.lifecycle.drop_inherited();
    tracing::info!(%addr, hub, "conflux node listening");
    let in_flight = node.lifecycle.in_flight();
    let stop = node.lifecycle.drain_started();
    let routes = node.routes();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, routes).with_graceful_shutdown(stop).await {
            tracing::error!("listener stopped: {e}");
        }
        drop(in_flight);
    });
    if let Some(takeover) = takeover {
        takeover.ready(|_| {}).await?;
    }
    node.resume_move().await?;
    let background = [tokio::spawn(node.clone().run_control()), tokio::spawn(sync.run())];
    update::remove_old_binaries(&exe);
    node.lifecycle.drain_started().await;
    tracing::info!("a newer conflux took over; finishing the requests already running");
    for task in background {
        task.abort();
    }
    node.lifecycle.drained().await;
    Ok(())
}

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::access::{self, Verdict};
use crate::api::{ApiError, UpdateRequest};
use crate::app::App;
use crate::config::{Config, Endpoints, Paths, Role};
use crate::lifecycle::Lifecycle;
use crate::setup::{self, ClientPaths};
use crate::tailnet::{self, Peer, TailnetSelf, Tailscale};
use crate::update::{UpdateStatus, Updater};
use crate::{dashboard, node, update};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const LINK_TIMEOUT: Duration = Duration::from_secs(15);
const LOCAL_ONLY: &str = "run this on the machine itself";
const RESCAN_TAILNET: Duration = Duration::from_secs(30);
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(2);

#[derive(Serialize)]
struct Found {
    name: String,
    address: IpAddr,
    os: String,
    state: &'static str,
    install: Option<String>,
}

#[derive(Serialize)]
struct FoundHub {
    name: String,
    address: IpAddr,
}

#[derive(Deserialize)]
pub struct LinkRequest {
    address: IpAddr,
}

fn url_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

fn address(peer: &Peer) -> Option<IpAddr> {
    let ips = &peer.machine.ips;
    ips.iter().find(|ip| ip.is_ipv4()).or(ips.first()).copied()
}

fn peer_name(peer: &Peer, address: IpAddr) -> String {
    peer.machine.short_name().unwrap_or_else(|| address.to_string())
}

async fn tailnet_status(app: &App) -> Result<Value, ApiError> {
    let tailscale = app.tailscale.clone();
    tokio::task::spawn_blocking(move || tailnet::status(&tailscale))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Tailscale is not running on the hub".into(),
            )
        })
}

async fn probe(app: &App, address: IpAddr) -> &'static str {
    let url = format!("http://{}/conflux/link", SocketAddr::new(address, app.cfg.port));
    let Ok(resp) = app.proxy_http.get(url).timeout(PROBE_TIMEOUT).send().await else {
        return "missing";
    };
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if v["state"] == "waiting" { "waiting" } else { "busy" }
}

pub async fn discover(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let status = tailnet_status(&app).await?;
    let known = app.machines.addresses();
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let install = dashboard::add_machine(&dashboard::hub_name(&app, host));
    let peers = tailnet::same_user_peers(&status)
        .into_iter()
        .filter(|p| !p.machine.ips.iter().any(|ip| known.contains(ip)))
        .filter_map(|p| Some((address(&p)?, p)));
    let mut found: Vec<Found> = futures_util::future::join_all(peers.map(|(address, peer)| {
        let app = app.clone();
        let install = &install;
        async move {
            let state = probe(&app, address).await;
            let key = if peer.os == "windows" {
                "windows_command"
            } else {
                "command"
            };
            Found {
                name: peer_name(&peer, address),
                address,
                install: (state == "missing").then(|| install[key].as_str().unwrap_or_default().to_string()),
                os: peer.os,
                state,
            }
        }
    }))
    .await;
    found.sort_by(|a, b| (a.state != "waiting", &a.name).cmp(&(b.state != "waiting", &b.name)));
    Ok(Json(json!({ "machines": found })))
}

pub async fn link(State(app): State<Arc<App>>, Json(req): Json<LinkRequest>) -> Result<Json<Value>, ApiError> {
    let status = tailnet_status(&app).await?;
    let peer = tailnet::same_user_peers(&status)
        .into_iter()
        .find(|p| p.machine.ips.contains(&req.address))
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "that machine is not one of yours on the tailnet".into(),
            )
        })?;
    let name = peer_name(&peer, req.address);
    let url = format!("http://{}/conflux/link", SocketAddr::new(req.address, app.cfg.port));
    let resp = app
        .proxy_http
        .post(url)
        .timeout(LINK_TIMEOUT)
        .send()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("could not reach {name}: {e}")))?;
    let code = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !code.is_success() {
        let reason = v["error"].as_str().unwrap_or("it did not accept the link");
        return Err(ApiError(StatusCode::BAD_GATEWAY, format!("{name}: {reason}")));
    }
    Ok(Json(json!({ "linked": name })))
}

pub fn become_waiting(paths: &Paths) -> Result<Option<Role>> {
    let cfg = Config::load(paths)?;
    if setup::load_joined(&paths.config_dir)?.is_some() || paths.has_accounts() {
        return Ok(Some(cfg.role));
    }
    Config {
        role: Role::Waiting,
        hub: None,
        ..cfg
    }
    .save(paths)?;
    Ok(None)
}

pub struct Waiting {
    port: u16,
    hub_port: u16,
    paths: Paths,
    clients: ClientPaths,
    tailscale: Tailscale,
    http: reqwest::Client,
    hostname: String,
    allowed_hosts: RwLock<Vec<String>>,
    accepting: tokio::sync::Mutex<()>,
    linked: watch::Sender<bool>,
    lifecycle: Arc<Lifecycle>,
    updater: Arc<Updater>,
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

impl Waiting {
    pub fn new(port: u16, paths: Paths, clients: ClientPaths, tailscale: Tailscale) -> Arc<Self> {
        Arc::new(Self {
            port,
            hub_port: node::hub_port(port),
            paths,
            clients,
            tailscale,
            http: crate::app::proxy_client(),
            hostname: tailnet::hostname(),
            allowed_hosts: RwLock::new(access::local_hosts(port)),
            accepting: tokio::sync::Mutex::new(()),
            linked: watch::channel(false).0,
            lifecycle: Arc::default(),
            updater: Updater::new(&Endpoints::from_env().releases),
        })
    }

    pub fn routes(self: &Arc<Self>) -> axum::Router {
        axum::Router::new()
            .route("/conflux/link", get(offer).post(accept))
            .route("/conflux/api/hubs", get(find_hubs))
            .route("/conflux/api/join", post(join_hub))
            .route("/conflux/api/status", get(waiting_status))
            .route("/conflux/api/update", post(update_self))
            .fallback(not_linked)
            .layer(axum::middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }

    fn stopped(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut linked = self.linked.subscribe();
        let replaced = self.lifecycle.drain_started();
        let lifecycle = self.lifecycle.clone();
        async move {
            tokio::select! {
                _ = linked.wait_for(|l| *l) => {}
                () = replaced => {}
            }
            lifecycle.close_listeners();
        }
    }

    fn serve_on(self: &Arc<Self>, listener: tokio::net::TcpListener) -> JoinHandle<()> {
        let routes = self.routes().into_make_service_with_connect_info::<SocketAddr>();
        let stop = self.stopped();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, routes).with_graceful_shutdown(stop).await {
                tracing::error!("listener stopped: {e}");
            }
        })
    }

    async fn listen_on_tailnet(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut bound = HashSet::new();
        let mut servers = Vec::new();
        let stopped = self.stopped();
        tokio::pin!(stopped);
        loop {
            let tailscale = self.tailscale.clone();
            let found = tokio::task::spawn_blocking(move || tailnet::status(&tailscale))
                .await
                .ok()
                .flatten()
                .map(|v| TailnetSelf::from_status(&v))
                .unwrap_or_default();
            let mut hosts = access::local_hosts(self.port);
            hosts.extend(found.host_headers(self.port));
            *self.allowed_hosts.write().unwrap() = hosts;
            for ip in found.ips {
                if bound.contains(&ip) {
                    continue;
                }
                let addr = SocketAddr::new(ip, self.port);
                match self.lifecycle.listen(addr) {
                    Ok(listener) => {
                        bound.insert(ip);
                        tracing::info!(%addr, "waiting to be linked on the tailnet");
                        servers.push(self.serve_on(listener));
                    }
                    Err(e) => tracing::warn!(%addr, "could not bind the tailnet address yet: {e}"),
                }
            }
            tokio::select! {
                () = &mut stopped => return servers,
                () = tokio::time::sleep(RESCAN_TAILNET) => {}
            }
        }
    }
}

async fn guard(State(w): State<Arc<Waiting>>, req: Request, next: Next) -> Response {
    let allowed = w.allowed_hosts.read().unwrap().clone();
    match access::check(&allowed, req.method(), req.uri().path(), req.headers()) {
        Verdict::Allow => next.run(req).await,
        Verdict::Deny(status, message) => refuse(status, message),
    }
}

async fn offer(State(w): State<Arc<Waiting>>) -> Json<Value> {
    Json(json!({ "state": "waiting", "hostname": w.hostname, "version": update::CURRENT }))
}

async fn waiting_status(State(w): State<Arc<Waiting>>) -> Json<Value> {
    let latest = w.updater.latest();
    let this_machine = json!({
        "hostname": w.hostname,
        "role": Role::Waiting,
        "version": update::CURRENT,
        "online": true,
        "outdated": latest.as_deref().is_some_and(|l| update::is_newer(l, update::CURRENT)),
        "update": w.updater.status(),
    });
    Json(json!({
        "role": Role::Waiting,
        "version": update::CURRENT,
        "pid": std::process::id(),
        "latest_version": latest,
        "machines": [this_machine],
    }))
}

async fn update_self(State(w): State<Arc<Waiting>>, Json(req): Json<UpdateRequest>) -> Response {
    let version = match w.updater.check_latest().await {
        Ok(version) => version,
        Err(e) => {
            return refuse(
                StatusCode::BAD_GATEWAY,
                format!("could not check for a new release: {e:#}"),
            );
        }
    };
    let mut skipped: Vec<Value> = req
        .machines
        .iter()
        .flatten()
        .filter(|name| **name != w.hostname)
        .map(|name| json!({ "machine": name, "reason": "unknown machine" }))
        .collect();
    let mut started = Vec::new();
    if req.machines.as_ref().is_none_or(|only| only.contains(&w.hostname)) {
        let _one_at_a_time = w.accepting.lock().await;
        if *w.linked.borrow() {
            return refuse(
                StatusCode::CONFLICT,
                "this machine was just linked to a hub. Run the update again.",
            );
        }
        let obstacle = if !update::is_newer(&version, update::CURRENT) {
            Some("already up to date")
        } else if !w.updater.claim(&version) {
            Some("already updating")
        } else {
            None
        };
        match obstacle {
            Some(reason) => skipped.push(json!({ "machine": w.hostname, "reason": reason })),
            None => {
                started.push(w.hostname.clone());
                tokio::spawn(
                    w.updater
                        .clone()
                        .run_claimed(version.clone(), w.lifecycle.clone(), w.clone()),
                );
            }
        }
    }
    Json(json!({ "version": version, "started": started, "skipped": skipped })).into_response()
}

async fn not_linked() -> Response {
    refuse(
        StatusCode::CONFLICT,
        "this machine is waiting to be linked to a hub. Run `conflux join` here to find the hub, or on the hub's dashboard, open Machines and click Find machines.",
    )
}

async fn accept(State(w): State<Arc<Waiting>>, ConnectInfo(from): ConnectInfo<SocketAddr>) -> Response {
    let tailscale = w.tailscale.clone();
    let caller = from.ip();
    let same_user = tokio::task::spawn_blocking(move || tailnet::is_same_user(&tailscale, caller))
        .await
        .unwrap_or(false);
    if !same_user {
        return refuse(
            StatusCode::FORBIDDEN,
            "the hub must belong to the same Tailscale user as this machine",
        );
    }
    w.link_to(&url_host(caller), json!({ "hostname": w.hostname })).await
}

async fn find_hubs(State(w): State<Arc<Waiting>>, ConnectInfo(from): ConnectInfo<SocketAddr>) -> Response {
    if !from.ip().is_loopback() {
        return refuse(StatusCode::FORBIDDEN, LOCAL_ONLY);
    }
    let Some(peers) = w.same_user_peers().await else {
        return no_tailscale();
    };
    let probes = peers.iter().filter_map(|peer| {
        let address = address(peer)?;
        let w = &w;
        Some(async move {
            w.is_hub(&url_host(address)).await.then(|| FoundHub {
                name: peer_name(peer, address),
                address,
            })
        })
    });
    let mut hubs: Vec<FoundHub> = futures_util::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect();
    hubs.sort_by(|a, b| a.name.cmp(&b.name));
    Json(json!({ "hubs": hubs })).into_response()
}

async fn join_hub(
    State(w): State<Arc<Waiting>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    Json(req): Json<LinkRequest>,
) -> Response {
    if !from.ip().is_loopback() {
        return refuse(StatusCode::FORBIDDEN, LOCAL_ONLY);
    }
    let Some(peers) = w.same_user_peers().await else {
        return no_tailscale();
    };
    let Some(peer) = peers.iter().find(|p| p.machine.ips.contains(&req.address)) else {
        return refuse(StatusCode::NOT_FOUND, "that machine is not one of yours on the tailnet");
    };
    let name = peer_name(peer, req.address);
    let by_address = url_host(req.address);
    if !w.is_hub(&by_address).await {
        return refuse(StatusCode::BAD_GATEWAY, format!("{name} is not running a conflux hub"));
    }
    w.link_to(&by_address, json!({ "hub": name })).await
}

fn no_tailscale() -> Response {
    refuse(
        StatusCode::SERVICE_UNAVAILABLE,
        "Tailscale is not running on this machine",
    )
}

impl Waiting {
    pub async fn run(self: Arc<Self>, listeners: Vec<tokio::net::TcpListener>) {
        let mut servers: Vec<_> = listeners.into_iter().map(|l| self.serve_on(l)).collect();
        servers.extend(self.clone().listen_on_tailnet().await);
        let aborts: Vec<_> = servers.iter().map(JoinHandle::abort_handle).collect();
        let mut finished = std::pin::pin!(futures_util::future::join_all(servers));
        if tokio::time::timeout(SHUTDOWN_LIMIT, &mut finished).await.is_err() {
            for abort in &aborts {
                abort.abort();
            }
            finished.await;
        }
    }

    async fn same_user_peers(&self) -> Option<Vec<Peer>> {
        let tailscale = self.tailscale.clone();
        let status = tokio::task::spawn_blocking(move || tailnet::status(&tailscale))
            .await
            .ok()??;
        Some(tailnet::same_user_peers(&status))
    }

    async fn is_hub(&self, host: &str) -> bool {
        let url = format!("http://{host}:{}/conflux/api/status", self.hub_port);
        let Ok(resp) = self.http.get(url).timeout(PROBE_TIMEOUT).send().await else {
            return false;
        };
        resp.json::<Value>().await.is_ok_and(|v| v["role"] == "hub")
    }

    async fn link_to(&self, hub: &str, linked: Value) -> Response {
        let _one_at_a_time = self.accepting.lock().await;
        if self.updater.status().as_ref().is_some_and(UpdateStatus::in_progress) {
            return refuse(
                StatusCode::CONFLICT,
                "this machine is updating conflux. Try again in a minute.",
            );
        }
        match self.become_node(hub) {
            Ok(None) => Json(linked).into_response(),
            Ok(Some(reason)) => refuse(StatusCode::CONFLICT, reason),
            Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }

    fn become_node(&self, hub: &str) -> Result<Option<String>> {
        let mut cfg = Config::load(&self.paths)?;
        if cfg.role != Role::Waiting {
            return Ok(Some(format!(
                "this machine is already linked to {}",
                cfg.hub.as_deref().unwrap_or("a hub")
            )));
        }
        if self.paths.has_accounts() {
            return Ok(Some("this machine has logged-in accounts".into()));
        }
        setup::apply(&self.clients, &self.paths.config_dir, self.port)?;
        cfg.role = Role::Node;
        cfg.hub = Some(hub.to_string());
        cfg.save(&self.paths)?;
        self.linked.send_replace(true);
        tracing::info!(hub, "linked to a hub");
        Ok(None)
    }
}

impl update::Handoff for Waiting {
    async fn pause(&self) -> (Value, mpsc::UnboundedReceiver<Value>) {
        (Value::Null, mpsc::unbounded_channel().1)
    }

    async fn resume(&self) {}
}

pub async fn serve(cfg: Config, paths: Paths) -> Result<bool> {
    let waiting = Waiting::new(cfg.port, paths, ClientPaths::from_env()?, tailnet::system_tailscale());
    let takeover = waiting.lifecycle.take_over().await?;
    let exe = update::running_exe()?;
    waiting.updater.replaces(exe.clone());
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = waiting.lifecycle.listen(addr).with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "conflux is waiting to be linked to a hub");
    if let Some(takeover) = takeover {
        takeover.ready(|_| {}).await?;
    }
    update::remove_old_binaries(&exe);
    let checks = tokio::spawn(waiting.updater.clone().run_checks());
    waiting.clone().run(vec![listener]).await;
    checks.abort();
    if waiting.lifecycle.is_draining() {
        tracing::info!("a newer conflux took over");
        return Ok(false);
    }
    Ok(true)
}

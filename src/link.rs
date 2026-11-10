use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::access::{self, Verdict};
use crate::api::ApiError;
use crate::app::App;
use crate::config::{Config, Paths, Role};
use crate::setup::{self, ClientPaths};
use crate::tailnet::{self, Peer, TailnetSelf, Tailscale};
use crate::{dashboard, update};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const LINK_TIMEOUT: Duration = Duration::from_secs(15);
const RESCAN_TAILNET: Duration = Duration::from_secs(30);

#[derive(Serialize)]
struct Found {
    name: String,
    address: IpAddr,
    os: String,
    state: &'static str,
    install: Option<String>,
}

#[derive(Deserialize)]
pub struct LinkRequest {
    address: IpAddr,
}

#[derive(Deserialize)]
struct AcceptRequest {
    hub: String,
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
    let Ok(resp) = app.control_http.get(url).timeout(PROBE_TIMEOUT).send().await else {
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
    let me = TailnetSelf::from_status(&status);
    let hub = me
        .ips
        .iter()
        .find(|ip| ip.is_ipv4())
        .or(me.ips.first())
        .map(|ip| url_host(*ip))
        .ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE, "the hub has no tailnet address".into()))?;
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
        .control_http
        .post(url)
        .json(&json!({ "hub": hub }))
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
    paths: Paths,
    clients: ClientPaths,
    tailscale: Tailscale,
    hostname: String,
    allowed_hosts: RwLock<Vec<String>>,
    accepting: tokio::sync::Mutex<()>,
    linked: watch::Sender<bool>,
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

impl Waiting {
    pub fn new(port: u16, paths: Paths, clients: ClientPaths, tailscale: Tailscale) -> Arc<Self> {
        Arc::new(Self {
            port,
            paths,
            clients,
            tailscale,
            hostname: tailnet::hostname(),
            allowed_hosts: RwLock::new(access::local_hosts(port)),
            accepting: tokio::sync::Mutex::new(()),
            linked: watch::channel(false).0,
        })
    }

    pub fn routes(self: &Arc<Self>) -> axum::Router {
        axum::Router::new()
            .route("/conflux/link", get(offer).post(accept))
            .route("/conflux/api/status", get(waiting_status))
            .fallback(not_linked)
            .layer(axum::middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }

    pub fn linked(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut linked = self.linked.subscribe();
        async move {
            let _ = linked.wait_for(|l| *l).await;
        }
    }

    fn serve_on(self: &Arc<Self>, listener: tokio::net::TcpListener) -> JoinHandle<()> {
        let routes = self.routes().into_make_service_with_connect_info::<SocketAddr>();
        let stop = self.linked();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, routes).with_graceful_shutdown(stop).await {
                tracing::error!("listener stopped: {e}");
            }
        })
    }

    async fn listen_on_tailnet(self: Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut bound = HashSet::new();
        let mut servers = Vec::new();
        let linked = self.linked();
        tokio::pin!(linked);
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
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        bound.insert(ip);
                        tracing::info!(%addr, "waiting to be linked on the tailnet");
                        servers.push(self.serve_on(listener));
                    }
                    Err(e) => tracing::warn!(%addr, "could not bind the tailnet address yet: {e}"),
                }
            }
            tokio::select! {
                () = &mut linked => return servers,
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

async fn waiting_status() -> Json<Value> {
    Json(json!({ "role": Role::Waiting, "version": update::CURRENT, "pid": std::process::id() }))
}

async fn not_linked() -> Response {
    refuse(
        StatusCode::CONFLICT,
        "this machine is waiting to be linked to a hub. On the hub's dashboard, open Machines and click Find machines, or run `conflux join <hub>` here.",
    )
}

async fn accept(
    State(w): State<Arc<Waiting>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    Json(req): Json<AcceptRequest>,
) -> Response {
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
    let _one_at_a_time = w.accepting.lock().await;
    match w.become_node(&req.hub) {
        Ok(None) => Json(json!({ "hostname": w.hostname })).into_response(),
        Ok(Some(reason)) => refuse(StatusCode::CONFLICT, reason),
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

impl Waiting {
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

pub async fn serve(cfg: Config, paths: Paths) -> Result<()> {
    let waiting = Waiting::new(cfg.port, paths, ClientPaths::from_env()?, tailnet::system_tailscale());
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let local = waiting.serve_on(listener);
    tracing::info!(%addr, "conflux is waiting to be linked to a hub");
    let mut servers = waiting.clone().listen_on_tailnet().await;
    servers.push(local);
    for server in servers {
        let _ = server.await;
    }
    Ok(())
}

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::body::Body;
use axum::extract::ws::{Message as ClientMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde::Serialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::access::{self, Verdict};
use crate::config::{Config, Paths};
use crate::transcript_sync::TranscriptSync;
use crate::{proxy, tailnet, ws};

pub const NODE_HEADER: &str = "x-conflux-node";
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, Serialize)]
pub struct HeartbeatStatus {
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

pub struct Node {
    pub port: u16,
    hub_port: u16,
    hub: RwLock<String>,
    paths: Paths,
    http: reqwest::Client,
    hostname: String,
    allowed_hosts: Vec<String>,
    heartbeat: Mutex<HeartbeatStatus>,
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

impl Node {
    pub fn new(hub: String, port: u16, paths: Paths) -> Arc<Self> {
        Self::with_hub_port(hub, port, port, paths)
    }

    pub fn with_hub_port(hub: String, hub_port: u16, port: u16, paths: Paths) -> Arc<Self> {
        Arc::new(Self {
            port,
            hub_port,
            hub: RwLock::new(hub),
            paths,
            http: crate::app::proxy_client(),
            hostname: tailnet::hostname(),
            allowed_hosts: access::local_hosts(port),
            heartbeat: Mutex::default(),
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
            .fallback(forward)
            .layer(axum::middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }

    pub async fn heartbeat_once(&self) -> Result<Option<String>> {
        let resp = self
            .http
            .post(format!("{}/conflux/heartbeat", self.hub_base("http")))
            .header("content-type", "application/json")
            .body(
                json!({
                    "hostname": self.hostname,
                    "version": env!("CARGO_PKG_VERSION"),
                    "role": "node",
                    "transcript_sync": Config::load(&self.paths).map(|c| c.transcript_sync).unwrap_or(false),
                })
                .to_string(),
            )
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let v: Value = resp.error_for_status()?.json().await?;
        Ok(v["hub"].as_str().map(str::to_string))
    }

    pub async fn follow_redirect(&self, new_hub: &str) -> Result<bool> {
        if new_hub == self.hub() {
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

    pub async fn run_heartbeat(self: Arc<Self>) {
        loop {
            let result = self.heartbeat_once().await;
            let mut status = HeartbeatStatus::default();
            match result {
                Ok(redirect) => {
                    status.last_ok = Some(Utc::now());
                    if let Some(h) = redirect
                        && let Err(e) = self.follow_redirect(&h).await
                    {
                        tracing::warn!("could not follow hub redirect: {e:#}");
                    }
                }
                Err(e) => status.last_error = Some(format!("{e:#}")),
            }
            let prev_ok = self.heartbeat.lock().unwrap().last_ok;
            status.last_ok = status.last_ok.or(prev_ok);
            *self.heartbeat.lock().unwrap() = status;
            tokio::time::sleep(HEARTBEAT_EVERY).await;
        }
    }
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
        "version": env!("CARGO_PKG_VERSION"),
        "heartbeat": node.heartbeat.lock().unwrap().clone(),
    }))
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
    let mut headers = copy_headers(&parts.headers, &[HOP_BY_HOP]);
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
    for (k, v) in copy_headers(&parts.headers, &[HOP_BY_HOP, HANDSHAKE]) {
        if let Some(k) = k {
            req.headers_mut().insert(k, v);
        }
    }
    req.headers_mut().insert(NODE_HEADER, node.node_header());
    match tokio_tungstenite::connect_async(req).await {
        Ok((upstream, _)) => upgrade.on_upgrade(move |client| pipe(client, upstream)),
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
) {
    let (mut up_tx, mut up_rx) = upstream.split();
    loop {
        tokio::select! {
            msg = client.recv() => {
                let Some(Ok(msg)) = msg else {
                    let _ = up_tx.send(tungstenite::Message::Close(None)).await;
                    break;
                };
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
                let closing = matches!(msg, tungstenite::Message::Close(_));
                let Some(out) = ws::to_client(msg) else { continue };
                if client.send(out).await.is_err() || closing {
                    break;
                }
            }
        }
    }
}

pub async fn serve(cfg: Config, paths: Paths) -> Result<()> {
    let hub = cfg
        .hub
        .clone()
        .context("this machine is a node but has no hub set; run `conflux join <hub>`")?;
    let node = Node::new(hub.clone(), cfg.port, paths);
    tokio::spawn(node.clone().run_heartbeat());
    let for_sync = node.clone();
    let sync = TranscriptSync::new(
        node.hostname.clone(),
        node.paths.clone(),
        &crate::setup::ClientPaths::from_env()?,
        Arc::new(move || for_sync.hub_base("http")),
    );
    tokio::spawn(sync.run());
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, hub, "conflux node listening");
    axum::serve(listener, node.routes()).await?;
    Ok(())
}

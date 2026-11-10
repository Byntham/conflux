#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use conflux::accounts::Provider;
use conflux::app::App;
use conflux::config::{Config, Endpoints, Paths};
use conflux::oauth::LoginTokens;
use serde_json::json;

#[derive(Clone)]
pub enum Reply {
    Json(u16, Vec<(&'static str, String)>, String),
    Raw(Vec<(&'static str, String)>, Vec<u8>),
    Endless,
    Delayed(Duration, Box<Reply>),
}

#[derive(Clone, Copy)]
pub enum WsScript {
    Normal,
    QuotaMidStream,
    SlowTurn,
    RejectHandshake429,
    RejectHandshake401,
}

#[derive(Default)]
pub struct Fake {
    pub by_token: Mutex<HashMap<String, VecDeque<Reply>>>,
    pub ws_by_token: Mutex<HashMap<String, VecDeque<WsScript>>>,
    pub refresh_replies: Mutex<VecDeque<Reply>>,
    pub hits: Mutex<Vec<(String, String)>>,
    pub ws_headers: Mutex<Vec<http::HeaderMap>>,
    pub http_headers: Mutex<Vec<http::HeaderMap>>,
    pub body_lengths: Mutex<Vec<usize>>,
    pub stream_dropped: Arc<AtomicBool>,
    pub release: Mutex<Option<(String, std::path::PathBuf)>>,
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn build(reply: Reply, dropped: Arc<AtomicBool>) -> Response {
    match reply {
        Reply::Json(status, headers, body) => {
            let mut r = Response::builder()
                .status(status)
                .header("content-type", "application/json");
            for (k, v) in headers {
                r = r.header(k, v);
            }
            r.body(Body::from(body)).unwrap()
        }
        Reply::Raw(headers, bytes) => {
            let mut r = Response::builder().status(200);
            for (k, v) in headers {
                r = r.header(k, v);
            }
            r.body(Body::from(bytes)).unwrap()
        }
        Reply::Delayed(..) => unreachable!("delays are handled before building"),
        Reply::Endless => {
            let flag = DropFlag(dropped);
            let s = futures_util::stream::unfold(flag, |flag| async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: tick\n\n")),
                    flag,
                ))
            });
            Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(s))
                .unwrap()
        }
    }
}

fn bearer(h: &http::HeaderMap) -> String {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim_start_matches("Bearer ")
        .to_string()
}

async fn fake_ws(fake: Arc<Fake>, req: Request) -> Response {
    let (mut parts, _) = req.into_parts();
    let token = bearer(&parts.headers);
    fake.ws_headers.lock().unwrap().push(parts.headers.clone());
    let script = fake
        .ws_by_token
        .lock()
        .unwrap()
        .get_mut(&token)
        .and_then(|q| q.pop_front())
        .unwrap_or(WsScript::Normal);
    if let WsScript::RejectHandshake429 = script {
        let body = json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 3600}}).to_string();
        return Response::builder().status(429).body(Body::from(body)).unwrap();
    }
    if let WsScript::RejectHandshake401 = script {
        return Response::builder()
            .status(401)
            .body(Body::from("token revoked"))
            .unwrap();
    }
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &()).await.unwrap();
    upgrade.on_upgrade(move |mut socket| async move {
        let mut n = 0;
        while let Some(Ok(msg)) = socket.recv().await {
            let Message::Text(_) = msg else { continue };
            n += 1;
            if let WsScript::SlowTurn = script {
                let id = format!("resp_{token}_{n}");
                let created = json!({"type": "response.created", "response": {"id": id}});
                if socket.send(Message::Text(created.to_string().into())).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
                let completed = json!({"type": "response.completed", "response": {"id": id}});
                if socket.send(Message::Text(completed.to_string().into())).await.is_err() {
                    return;
                }
                continue;
            }
            let frames = match script {
                WsScript::QuotaMidStream => vec![json!({
                    "type": "error", "status": 429,
                    "error": {"type": "usage_limit_reached", "message": "limit", "resets_in_seconds": 600}
                })],
                _ => vec![
                    json!({"type": "response.created", "response": {"id": format!("resp_{token}_{n}")}}),
                    json!({"type": "codex.rate_limits", "rate_limits": {
                        "primary": {"used_percent": 33.0, "window_minutes": 300, "reset_after_seconds": 100}}}),
                    json!({"type": "response.completed", "response": {"id": format!("resp_{token}_{n}"), "token": token}}),
                ],
            };
            for f in frames {
                if socket.send(Message::Text(f.to_string().into())).await.is_err() {
                    return;
                }
            }
        }
    })
}

async fn fake_handler(State(fake): State<Arc<Fake>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let token = bearer(req.headers());
    fake.hits.lock().unwrap().push((path.clone(), token.clone()));
    fake.http_headers.lock().unwrap().push(req.headers().clone());
    if req.headers().get("upgrade").is_some() {
        return fake_ws(fake, req).await.into_response();
    }
    if let Some(rest) = path.strip_prefix("/releases/") {
        let release = fake.release.lock().unwrap().clone();
        return match release {
            Some((version, _)) if rest == "latest" => Response::builder()
                .status(302)
                .header("location", format!("/releases/tag/v{version}"))
                .body(Body::empty())
                .unwrap(),
            Some((_, binary)) if rest.starts_with("download/") => {
                Response::new(Body::from(tokio::fs::read(binary).await.unwrap()))
            }
            Some(_) => Response::new(Body::empty()),
            None => Response::builder().status(404).body(Body::empty()).unwrap(),
        };
    }
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    fake.body_lengths.lock().unwrap().push(body.len());
    let reply = if path == "/v1/oauth/token" || path == "/oauth/token" {
        fake.refresh_replies.lock().unwrap().pop_front()
    } else {
        fake.by_token
            .lock()
            .unwrap()
            .get_mut(&token)
            .and_then(|q| q.pop_front())
    };
    let dropped = fake.stream_dropped.clone();
    match reply {
        Some(Reply::Delayed(wait, inner)) => {
            tokio::time::sleep(wait).await;
            build(*inner, dropped)
        }
        Some(r) => build(r, dropped),
        None => build(
            Reply::Json(200, vec![], json!({"ok": true, "token": token}).to_string()),
            dropped,
        ),
    }
}

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub app: Arc<App>,
    pub fake: Arc<Fake>,
    pub base: String,
    pub port: u16,
    pub http: reqwest::Client,
}

impl Harness {
    pub async fn new() -> Self {
        Self::on(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()).await
    }

    pub async fn on(listener: tokio::net::TcpListener) -> Self {
        let fake = Arc::new(Fake::default());
        let fake_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_addr = fake_listener.local_addr().unwrap();
        let fake_app = axum::Router::new().fallback(fake_handler).with_state(fake.clone());
        tokio::spawn(async move { axum::serve(fake_listener, fake_app).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let cfg = Config {
            port,
            ..Config::default()
        };
        let app = App::new(
            cfg,
            &Paths::under(dir.path()),
            Endpoints::all_at(&format!("http://{fake_addr}")),
        )
        .unwrap();
        let routes = app
            .routes()
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        Self {
            dir,
            app,
            fake,
            base: format!("http://{addr}"),
            port,
            http: reqwest::Client::new(),
        }
    }

    pub async fn add(&self, provider: Provider, name: &str) -> String {
        let e = self
            .app
            .refresher
            .install_login(LoginTokens {
                provider,
                identity: name.into(),
                email: Some(format!("{name}@example.com")),
                plan: None,
                chatgpt_account_id: (provider == Provider::Codex).then(|| format!("acct-{name}")),
                access_token: format!("at-{name}"),
                refresh_token: format!("rt-{name}"),
                expires_at: Utc::now() + chrono::Duration::hours(8),
                id_token: None,
            })
            .await
            .unwrap();
        e.id.clone()
    }

    pub fn reply(&self, token: &str, r: Reply) {
        self.fake
            .by_token
            .lock()
            .unwrap()
            .entry(token.to_string())
            .or_default()
            .push_back(r);
    }

    pub fn ws_script(&self, token: &str, s: WsScript) {
        self.fake
            .ws_by_token
            .lock()
            .unwrap()
            .entry(token.to_string())
            .or_default()
            .push_back(s);
    }

    pub fn hits_for(&self, token: &str) -> usize {
        self.fake
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t)| t == token)
            .count()
    }

    pub fn claude(&self, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/v1/messages", self.base))
            .header("user-agent", "claude-cli/2.1.280 (external, cli)")
            .header("x-app", "cli")
            .header("authorization", "Bearer conflux")
            .json(&body)
    }
}

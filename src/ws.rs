use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::FromRequestParts;
use axum::extract::ws::{CloseFrame, Message as ClientMsg, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as UpstreamCloseFrame;
use tokio_tungstenite::tungstenite::{self, Message as UpstreamMsg};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::accounts::{AccountEntry, Provider};
use crate::app::App;
use crate::proxy::{self, Rejection};
use crate::refresh::RefreshResult;
use crate::requests::{self, TokenUsage};
use crate::router::{self, NoAccount, RouteRequest};

pub const RESPONSES_WEBSOCKETS_BETA: &str = "responses_websockets=2026-02-06";
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

type Upstream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn handle(app: Arc<App>, mut parts: http::request::Parts, path: String) -> Response {
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let headers = parts.headers.clone();
    let query = parts.uri.query().map(str::to_string);
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            relay(app, socket, headers, path, query).await;
        })
}

pub fn error_frame(status: u16, kind: &str, message: &str, resets_in_seconds: Option<i64>) -> String {
    let mut error = json!({ "type": kind, "message": message });
    if let Some(s) = resets_in_seconds {
        error["resets_in_seconds"] = json!(s);
    }
    json!({ "type": "error", "status": status, "error": error }).to_string()
}

fn no_account_frame(why: NoAccount) -> String {
    match why {
        NoAccount::NoneConfigured => error_frame(
            503,
            "server_error",
            "conflux: no codex account is logged in on the hub. Run `conflux login codex` there.",
            None,
        ),
        NoAccount::AllLimited(soonest) => {
            let secs = soonest.map(|t| (t - Utc::now()).num_seconds().max(1));
            error_frame(
                429,
                "usage_limit_reached",
                "conflux: every codex account is rate limited.",
                secs,
            )
        }
    }
}

async fn close_with(client: &mut WebSocket, text: String) {
    let _ = client.send(ClientMsg::Text(text.into())).await;
    let _ = client.send(ClientMsg::Close(None)).await;
}

pub fn first_frame_route(headers: &HeaderMap, first: &str) -> RouteRequest {
    let v: Value = serde_json::from_str(first).unwrap_or(Value::Null);
    let model = v["model"]
        .as_str()
        .or_else(|| v["response"]["model"].as_str())
        .map(str::to_string);
    let previous = v["previous_response_id"]
        .as_str()
        .or_else(|| v["response"]["previous_response_id"].as_str())
        .map(str::to_string);
    RouteRequest {
        provider: Provider::Codex,
        model,
        session: proxy::codex_session(headers, &v),
        previous_response_id: previous,
    }
}

const WS_HANDSHAKE_HEADERS: &[&str] = &[
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-accept",
];

pub fn upstream_headers(client: &HeaderMap, account: &AccountEntry) -> HeaderMap {
    let mut h = proxy::forward_request_headers(client);
    for name in WS_HANDSHAKE_HEADERS {
        h.remove(*name);
    }
    proxy::apply_account_auth(Provider::Codex, account, &mut h);
    let has_ws_beta = h
        .get("openai-beta")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("responses_websockets"));
    if !has_ws_beta {
        h.insert("openai-beta", HeaderValue::from_static(RESPONSES_WEBSOCKETS_BETA));
    }
    h
}

fn ws_url(http_url: &str) -> String {
    if let Some(rest) = http_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http_url.to_string()
    }
}

enum ConnectError {
    Rejected {
        status: u16,
        headers: HeaderMap,
        body: Vec<u8>,
    },
    NotSent(String),
    Failed(String),
}

async fn connect(
    app: &App,
    account: &AccountEntry,
    client_headers: &HeaderMap,
    url: &str,
) -> Result<Upstream, ConnectError> {
    let mut req = url
        .into_client_request()
        .map_err(|e| ConnectError::Failed(e.to_string()))?;
    for (k, v) in upstream_headers(client_headers, account) {
        if let Some(k) = k {
            req.headers_mut().insert(k, v);
        }
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, resp)) => {
            app.usage.observe_headers(&account.id, Provider::Codex, resp.headers());
            Ok(ws)
        }
        Err(tungstenite::Error::Http(resp)) => Err(ConnectError::Rejected {
            status: resp.status().as_u16(),
            headers: resp.headers().clone(),
            body: resp.body().clone().unwrap_or_default(),
        }),
        Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            Err(ConnectError::NotSent(e.to_string()))
        }
        Err(e) => Err(ConnectError::Failed(e.to_string())),
    }
}

async fn stop_requested(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

async fn relay(app: Arc<App>, mut client: WebSocket, headers: HeaderMap, path: String, query: Option<String>) {
    let first = loop {
        match client.recv().await {
            Some(Ok(ClientMsg::Text(t))) => break t.to_string(),
            Some(Ok(ClientMsg::Binary(b))) => break String::from_utf8_lossy(&b).to_string(),
            Some(Ok(ClientMsg::Ping(_) | ClientMsg::Pong(_))) => continue,
            _ => return,
        }
    };
    let route = first_frame_route(&headers, &first);
    let url = ws_url(&proxy::upstream_url(&app, Provider::Codex, &path, query.as_deref()));

    let mut tried = HashSet::new();
    let mut refreshed = HashSet::new();
    let mut last_rejection: Option<String> = None;
    let (upstream, account) = loop {
        let account = match app.router.pick(&route, &tried) {
            Ok(a) => a,
            Err(why) => return close_with(&mut client, last_rejection.unwrap_or_else(|| no_account_frame(why))).await,
        };
        if !app.accepting_requests() {
            let frame = error_frame(503, "server_error", proxy::HUB_NOT_ACTIVE, None);
            return close_with(&mut client, frame).await;
        }
        let sent_generation = account.credentials().generation;
        match connect(&app, &account, &headers, &url).await {
            Ok(ws) => break (ws, account),
            Err(ConnectError::Rejected { status: 401, body, .. }) => {
                let token_renewed = refreshed.insert(account.id.clone())
                    && app.refresher.refresh_rejected(&account, sent_generation).await == RefreshResult::Refreshed;
                if !token_renewed {
                    app.router.cool(
                        &account.id,
                        router::ACCOUNT,
                        Utc::now() + chrono::Duration::seconds(60),
                        "upstream 401",
                    );
                    tried.insert(account.id.clone());
                    last_rejection = Some(error_frame(401, "server_error", &String::from_utf8_lossy(&body), None));
                }
            }
            Err(ConnectError::Rejected {
                status: 429,
                headers: h,
                body,
            }) => {
                let rejection = proxy::classify_codex_429(&h, &body, route.model.as_deref(), Utc::now());
                apply_rejection(&app, &account, &route, &rejection);
                tried.insert(account.id.clone());
                last_rejection = Some(String::from_utf8_lossy(&body).to_string()).filter(|s| !s.is_empty());
            }
            Err(ConnectError::Rejected { status, body, .. }) => {
                let text = String::from_utf8_lossy(&body).to_string();
                tracing::warn!(account = %account.id, status, "upstream websocket handshake rejected");
                return close_with(&mut client, error_frame(status, "server_error", &text, None)).await;
            }
            Err(ConnectError::NotSent(e)) => {
                tracing::warn!(account = %account.id, "upstream websocket connect failed: {e}");
                tried.insert(account.id.clone());
                last_rejection = Some(error_frame(
                    502,
                    "server_error",
                    "conflux: could not reach the upstream",
                    None,
                ));
            }
            Err(ConnectError::Failed(e)) => {
                tracing::warn!(account = %account.id, "upstream websocket failed: {e}");
                return close_with(
                    &mut client,
                    error_frame(502, "server_error", "conflux: upstream websocket failed", None),
                )
                .await;
            }
        }
    };

    let turn = Turn {
        machine: proxy::machine_name(&app, &headers),
        started: std::cell::Cell::new(Instant::now()),
    };
    let (mut up_tx, mut up_rx) = upstream.split();
    if up_tx.send(UpstreamMsg::Text(first.into())).await.is_err() {
        return close_with(
            &mut client,
            error_frame(502, "server_error", "conflux: upstream closed", None),
        )
        .await;
    }

    let mut shutdown = app.ws_shutdown.subscribe();
    loop {
        tokio::select! {
            () = stop_requested(&mut shutdown) => {
                let _ = client.send(ClientMsg::Close(None)).await;
                let _ = up_tx.send(UpstreamMsg::Close(None)).await;
                break;
            }
            msg = client.recv() => {
                let Some(Ok(msg)) = msg else {
                    let _ = up_tx.send(UpstreamMsg::Close(None)).await;
                    break;
                };
                let closing = matches!(msg, ClientMsg::Close(_));
                if matches!(msg, ClientMsg::Text(_)) {
                    turn.started.set(Instant::now());
                }
                if up_tx.send(to_upstream(msg)).await.is_err() || closing {
                    break;
                }
            }
            msg = up_rx.next() => {
                let Some(Ok(msg)) = msg else {
                    let _ = client.send(ClientMsg::Close(None)).await;
                    break;
                };
                let mut quota_hit = false;
                if let UpstreamMsg::Text(t) = &msg {
                    quota_hit = inspect_event(&app, &account, &route, &turn, t.as_str());
                }
                let closing = matches!(msg, UpstreamMsg::Close(_));
                let Some(out) = to_client(msg) else { continue };
                if client.send(out).await.is_err() || closing {
                    break;
                }
                if quota_hit {
                    let _ = client.send(ClientMsg::Close(None)).await;
                    let _ = up_tx.send(UpstreamMsg::Close(None)).await;
                    break;
                }
            }
        }
    }
}

fn apply_rejection(app: &App, account: &AccountEntry, route: &RouteRequest, rejection: &Rejection) {
    match rejection {
        Rejection::Quota { scope, until, reason } => {
            app.router.cool(&account.id, scope, *until, reason);
            if let Some(s) = &route.session {
                app.router.unpin(Provider::Codex, s);
            }
        }
        Rejection::Short { scope, until } => app.router.cool(&account.id, scope, *until, "short rate limit"),
    }
}

pub fn rate_limit_headers(event: &Value) -> HeaderMap {
    let mut h = HeaderMap::new();
    for key in ["primary", "secondary"] {
        let w = &event["rate_limits"][key];
        for (field, suffix) in [
            ("used_percent", "used-percent"),
            ("window_minutes", "window-minutes"),
            ("reset_after_seconds", "reset-after-seconds"),
            ("reset_at", "reset-at"),
        ] {
            let v = &w[field];
            let text = v
                .as_f64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(str::to_string));
            if let (Some(text), Ok(name)) = (
                text,
                HeaderName::from_bytes(format!("x-codex-{key}-{suffix}").as_bytes()),
            ) && let Ok(val) = HeaderValue::from_str(&text)
            {
                h.insert(name, val);
            }
        }
    }
    h
}

struct Turn {
    machine: String,
    started: std::cell::Cell<Instant>,
}

fn inspect_event(app: &App, account: &AccountEntry, route: &RouteRequest, turn: &Turn, text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match v["type"].as_str() {
        Some("response.created") => {
            if let Some(id) = v["response"]["id"].as_str() {
                app.router.record_response_owner(id, &account.id);
            }
            false
        }
        Some("response.completed") => {
            let mut usage = TokenUsage::default();
            requests::apply_event(Provider::Codex, &mut usage, &v);
            let record = proxy::log_attempt(
                app,
                Provider::Codex,
                account,
                route,
                &turn.machine,
                200,
                turn.started.get(),
            );
            app.requests.set_usage(record, usage);
            app.requests.set_transport(record, "ws");
            false
        }
        Some("codex.rate_limits") => {
            app.usage
                .observe_headers(&account.id, Provider::Codex, &rate_limit_headers(&v));
            false
        }
        Some("error" | "response.failed") if proxy::is_codex_usage_limit(&v) => {
            let now = Utc::now();
            let until = proxy::codex_reset(&v, now).unwrap_or(now + chrono::Duration::minutes(15));
            let rejection = Rejection::Quota {
                scope: router::ACCOUNT.into(),
                until,
                reason: "Codex usage limit".into(),
            };
            apply_rejection(app, account, route, &rejection);
            true
        }
        _ => false,
    }
}

pub fn to_upstream(msg: ClientMsg) -> UpstreamMsg {
    match msg {
        ClientMsg::Text(t) => UpstreamMsg::Text(t.as_str().into()),
        ClientMsg::Binary(b) => UpstreamMsg::Binary(b),
        ClientMsg::Ping(b) => UpstreamMsg::Ping(b),
        ClientMsg::Pong(b) => UpstreamMsg::Pong(b),
        ClientMsg::Close(frame) => UpstreamMsg::Close(frame.map(|f| UpstreamCloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
    }
}

pub fn to_client(msg: UpstreamMsg) -> Option<ClientMsg> {
    Some(match msg {
        UpstreamMsg::Text(t) => ClientMsg::Text(t.as_str().into()),
        UpstreamMsg::Binary(b) => ClientMsg::Binary(b),
        UpstreamMsg::Ping(b) => ClientMsg::Ping(b),
        UpstreamMsg::Pong(b) => ClientMsg::Pong(b),
        UpstreamMsg::Close(frame) => ClientMsg::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
        UpstreamMsg::Frame(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_routing() {
        let mut h = HeaderMap::new();
        h.insert("session_id", HeaderValue::from_static("s1"));
        let r = first_frame_route(
            &h,
            r#"{"type":"response.create","model":"gpt-6.1-sol","previous_response_id":"resp_1"}"#,
        );
        assert_eq!(r.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(r.previous_response_id.as_deref(), Some("resp_1"));
        assert_eq!(r.session.as_deref(), Some("s1"));
    }

    #[test]
    fn rate_limit_event_becomes_headers() {
        let ev = json!({"type": "codex.rate_limits", "rate_limits": {
            "primary": {"used_percent": 12.5, "window_minutes": 300, "reset_after_seconds": 60},
            "secondary": {"used_percent": 40, "window_minutes": 10080}
        }});
        let h = rate_limit_headers(&ev);
        assert_eq!(h.get("x-codex-primary-used-percent").unwrap(), "12.5");
        assert_eq!(h.get("x-codex-secondary-window-minutes").unwrap(), "10080");
    }

    #[test]
    fn ws_scheme() {
        assert_eq!(ws_url("https://chatgpt.com/x"), "wss://chatgpt.com/x");
        assert_eq!(ws_url("http://127.0.0.1:1/x"), "ws://127.0.0.1:1/x");
    }

    #[test]
    fn error_frame_shape() {
        let v: Value = serde_json::from_str(&error_frame(429, "usage_limit_reached", "m", Some(5))).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["status"], 429);
        assert!(proxy::is_codex_usage_limit(&v));
    }
}

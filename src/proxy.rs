use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use serde_json::{Value, json};

use crate::accounts::{AccountEntry, Provider};
use crate::app::App;
use crate::refresh::RefreshResult;
use crate::requests::{self, BodyScanner, RequestRecord, ResponseTap, TokenUsage};
use crate::router::{self, NoAccount, RouteRequest};

pub const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;
const MAX_REJECTION_BODY: usize = 1024 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";
pub const HUB_NOT_ACTIVE: &str = "conflux: the hub is moving to another machine. Try again in a minute.";

pub fn provider_for(path: &str, user_agent: &str) -> Option<Provider> {
    if path.starts_with("/v1/messages") {
        Some(Provider::Claude)
    } else if path.starts_with("/backend-api/codex/") {
        Some(Provider::Codex)
    } else if path.starts_with("/v1/") {
        Some(if is_claude_cli(user_agent) {
            Provider::Claude
        } else {
            Provider::Codex
        })
    } else {
        None
    }
}

fn is_claude_cli(user_agent: &str) -> bool {
    user_agent.starts_with("claude-cli/")
}

pub fn is_claude_code(h: &HeaderMap) -> bool {
    let ua = h.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let app = h.get("x-app").and_then(|v| v.to_str().ok()).unwrap_or("");
    is_claude_cli(ua) && app.eq_ignore_ascii_case("cli")
}

pub fn upstream_url(app: &App, provider: Provider, path: &str, query: Option<&str>) -> String {
    let q = query.map(|q| format!("?{q}")).unwrap_or_default();
    match provider {
        Provider::Claude => format!("{}{path}{q}", app.ep.anthropic_api),
        Provider::Codex => {
            let rest = path
                .strip_prefix("/backend-api/codex")
                .or_else(|| path.strip_prefix("/v1"))
                .unwrap_or(path);
            format!("{}{rest}{q}", app.ep.codex_api)
        }
    }
}

const DROPPED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "chatgpt-account-id",
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "cookie",
    "forwarded",
    "content-length",
    "origin",
];

const DROPPED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "content-length",
];

fn is_dropped(name: &str, list: &[&str]) -> bool {
    list.contains(&name)
        || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
        || name.starts_with("x-conflux-")
}

pub fn forward_request_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        if !is_dropped(k.as_str(), DROPPED_REQUEST_HEADERS) {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

fn forward_response_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in src {
        if !is_dropped(k.as_str(), DROPPED_RESPONSE_HEADERS) {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

pub fn apply_account_auth(provider: Provider, account: &AccountEntry, h: &mut HeaderMap) {
    let creds = account.credentials();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", creds.grant.access_token)) {
        h.insert(header::AUTHORIZATION, v);
    }
    match provider {
        Provider::Claude => {
            let beta = h
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let has = beta.split(',').any(|b| b.trim() == CLAUDE_OAUTH_BETA);
            let merged = match (has, beta.trim().is_empty()) {
                (true, _) => beta,
                (false, true) => CLAUDE_OAUTH_BETA.to_string(),
                (false, false) => format!("{beta},{CLAUDE_OAUTH_BETA}"),
            };
            if let Ok(v) = HeaderValue::from_str(&merged) {
                h.insert("anthropic-beta", v);
            }
        }
        Provider::Codex => {
            if let Some(acct) = creds
                .chatgpt_account_id
                .as_deref()
                .and_then(|a| HeaderValue::from_str(a).ok())
            {
                h.insert("chatgpt-account-id", acct);
            }
        }
    }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn claude_session(h: &HeaderMap, body: &Value) -> Option<String> {
    if let Some(s) = header_str(h, "x-claude-code-session-id") {
        return Some(s.to_string());
    }
    let user_id = body["metadata"]["user_id"].as_str()?;
    if let Ok(v) = serde_json::from_str::<Value>(user_id)
        && let Some(s) = v["session_id"].as_str()
    {
        return Some(s.to_string());
    }
    user_id.rsplit_once("_session_").map(|(_, s)| s.to_string())
}

pub fn codex_session(h: &HeaderMap, body: &Value) -> Option<String> {
    for name in [
        "session_id",
        "session-id",
        "conversation_id",
        "conversation-id",
        "thread-id",
        "thread_id",
    ] {
        if let Some(s) = header_str(h, name) {
            return Some(s.to_string());
        }
    }
    if let Some(meta) = header_str(h, "x-codex-turn-metadata").and_then(|m| serde_json::from_str::<Value>(m).ok()) {
        for key in ["session_id", "thread_id"] {
            if let Some(s) = meta[key].as_str() {
                return Some(s.to_string());
            }
        }
    }
    body["prompt_cache_key"].as_str().map(str::to_string)
}

pub fn route_request(provider: Provider, h: &HeaderMap, body: &[u8]) -> RouteRequest {
    let encoded = h.get(header::CONTENT_ENCODING).is_some();
    let json: Value = if encoded {
        Value::Null
    } else {
        serde_json::from_slice(body).unwrap_or(Value::Null)
    };
    RouteRequest {
        provider,
        model: json["model"].as_str().map(str::to_string),
        session: match provider {
            Provider::Claude => claude_session(h, &json),
            Provider::Codex => codex_session(h, &json),
        },
        previous_response_id: json["previous_response_id"].as_str().map(str::to_string),
    }
}

#[derive(Debug, PartialEq)]
pub enum Rejection {
    Quota {
        scope: String,
        until: DateTime<Utc>,
        reason: String,
    },
    Short {
        scope: String,
        until: DateTime<Utc>,
    },
}

fn parse_epoch_or_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(n) = raw.parse::<f64>() {
        let secs = if n > 1e12 { n / 1000.0 } else { n };
        return Utc.timestamp_opt(secs as i64, 0).single();
    }
    DateTime::parse_from_rfc3339(raw).ok().map(|d| d.with_timezone(&Utc))
}

fn retry_after(h: &HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    header_str(h, "retry-after")
        .and_then(|v| v.parse::<i64>().ok())
        .map(|s| now + chrono::Duration::seconds(s))
}

fn model_scope_or_account(model: Option<&str>) -> String {
    model
        .map(router::model_scope)
        .unwrap_or_else(|| router::ACCOUNT.to_string())
}

pub fn classify_claude_429(h: &HeaderMap, model: Option<&str>, now: DateTime<Utc>) -> Rejection {
    let status = |tag: &str| header_str(h, &format!("anthropic-ratelimit-unified-{tag}status")).map(str::to_lowercase);
    let reset =
        |tag: &str| header_str(h, &format!("anthropic-ratelimit-unified-{tag}reset")).and_then(parse_epoch_or_rfc3339);
    let util_full = |tag: &str| {
        header_str(h, &format!("anthropic-ratelimit-unified-{tag}-utilization"))
            .and_then(|v| v.parse::<f64>().ok())
            .is_some_and(|u| u >= 1.0)
    };
    let rejected_5h = status("5h-").as_deref() == Some("rejected");
    let rejected_7d = status("7d-").as_deref() == Some("rejected");
    let rejected_any = status("").as_deref() == Some("rejected");
    let fallback = retry_after(h, now).unwrap_or(now + chrono::Duration::minutes(5));

    if rejected_5h || rejected_7d || (rejected_any && (util_full("5h") || util_full("7d"))) {
        let mut resets = Vec::new();
        if rejected_5h || util_full("5h") {
            resets.extend(reset("5h-"));
        }
        if rejected_7d || util_full("7d") {
            resets.extend(reset("7d-"));
        }
        let until = resets.into_iter().max().or_else(|| reset("")).unwrap_or(fallback);
        return Rejection::Quota {
            scope: router::ACCOUNT.into(),
            until,
            reason: "Claude subscription limit".into(),
        };
    }
    if rejected_any {
        let until = reset("7d_oi-").or_else(|| reset("")).unwrap_or(fallback);
        return Rejection::Quota {
            scope: model_scope_or_account(model),
            until,
            reason: "Claude model-scoped or overage limit".into(),
        };
    }
    Rejection::Short {
        scope: model_scope_or_account(model),
        until: retry_after(h, now).unwrap_or(now + chrono::Duration::seconds(30)),
    }
}

pub fn codex_error_object(v: &Value) -> Option<&Value> {
    [&v["error"], &v["body"]["error"], &v["response"]["error"], v]
        .into_iter()
        .find(|e| e["type"].is_string() || e["code"].is_string())
}

pub fn is_codex_usage_limit(v: &Value) -> bool {
    codex_error_object(v).is_some_and(|e| {
        e["type"].as_str() == Some("usage_limit_reached") || e["code"].as_str() == Some("usage_limit_reached")
    })
}

pub fn codex_reset(v: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let e = codex_error_object(v)?;
    if let Some(t) = e["resets_at"].as_f64() {
        return Utc.timestamp_opt(t as i64, 0).single();
    }
    e["resets_in_seconds"]
        .as_i64()
        .map(|s| now + chrono::Duration::seconds(s))
}

pub fn classify_codex_429(h: &HeaderMap, body: &[u8], model: Option<&str>, now: DateTime<Utc>) -> Rejection {
    let plain = requests::decompressed(header_str(h, "content-encoding"), body);
    let v: Value = serde_json::from_slice(&plain).unwrap_or(Value::Null);
    if is_codex_usage_limit(&v) {
        let until = codex_reset(&v, now)
            .or_else(|| retry_after(h, now))
            .unwrap_or(now + chrono::Duration::minutes(15));
        return Rejection::Quota {
            scope: router::ACCOUNT.into(),
            until,
            reason: "Codex usage limit".into(),
        };
    }
    Rejection::Short {
        scope: model_scope_or_account(model),
        until: retry_after(h, now).unwrap_or(now + chrono::Duration::seconds(30)),
    }
}

fn json_response(status: StatusCode, body: Value, extra: Option<(HeaderName, String)>) -> Response {
    let mut r = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((k, v)) = extra {
        r = r.header(k, v);
    }
    r.body(Body::from(body.to_string())).expect("valid response")
}

pub fn provider_error(provider: Provider, status: StatusCode, kind: &str, message: &str) -> Response {
    let body = match provider {
        Provider::Claude => json!({"type": "error", "error": {"type": kind, "message": message}}),
        Provider::Codex => json!({"error": {"type": kind, "message": message}}),
    };
    json_response(status, body, None)
}

fn no_account_response(provider: Provider, why: NoAccount) -> Response {
    match why {
        NoAccount::NoneConfigured => provider_error(
            provider,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            &format!("conflux: no {provider} account is logged in on the hub. Run `conflux login {provider}` there."),
        ),
        NoAccount::AllLimited(soonest) => {
            let now = Utc::now();
            let secs = soonest.map(|t| (t - now).num_seconds().max(1));
            let message = match soonest {
                Some(t) => format!("conflux: every {provider} account is rate limited. The soonest frees up at {t}."),
                None => format!("conflux: every {provider} account is rate limited."),
            };
            let body = match provider {
                Provider::Claude => json!({"type": "error", "error": {"type": "rate_limit_error", "message": message}}),
                Provider::Codex => json!({"error": {
                    "type": "usage_limit_reached",
                    "message": message,
                    "resets_in_seconds": secs,
                }}),
            };
            json_response(
                StatusCode::TOO_MANY_REQUESTS,
                body,
                secs.map(|s| (header::RETRY_AFTER, s.to_string())),
            )
        }
    }
}

fn buffered_response(status: StatusCode, headers: &HeaderMap, body: Bytes) -> Response {
    let mut r = Response::builder().status(status);
    if let Some(h) = r.headers_mut() {
        *h = forward_response_headers(headers);
    }
    r.body(Body::from(body)).expect("valid response")
}

async fn read_limited(resp: reqwest::Response, limit: usize) -> Bytes {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = limit.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= limit {
            break;
        }
    }
    Bytes::from(out)
}

struct Attempt<'a> {
    app: &'a App,
    provider: Provider,
    method: &'a http::Method,
    url: String,
    headers: &'a HeaderMap,
    body: &'a Bytes,
}

impl Attempt<'_> {
    async fn send(&self, account: &AccountEntry) -> Result<reqwest::Response, reqwest::Error> {
        let mut h = forward_request_headers(self.headers);
        apply_account_auth(self.provider, account, &mut h);
        self.app
            .proxy_http
            .request(self.method.clone(), &self.url)
            .headers(h)
            .body(self.body.clone())
            .send()
            .await
    }
}

pub fn machine_name(app: &App, h: &HeaderMap) -> String {
    header_str(h, crate::node::NODE_HEADER)
        .unwrap_or(&app.hostname)
        .to_string()
}

pub fn log_attempt(
    app: &App,
    provider: Provider,
    account: &AccountEntry,
    route: &RouteRequest,
    machine: &str,
    status: u16,
    started: Instant,
) -> u64 {
    app.requests.add(RequestRecord {
        id: 0,
        at: Utc::now(),
        machine: machine.to_string(),
        provider,
        account: account.id.clone(),
        email: account.credentials().email,
        model: route.model.clone(),
        session: route.session.clone(),
        transport: "http".into(),
        status,
        latency_ms: started.elapsed().as_millis() as u64,
        usage: TokenUsage::default(),
    })
}

pub async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let ua = header_str(&parts.headers, "user-agent").unwrap_or("").to_string();
    let Some(provider) = provider_for(&path, &ua) else {
        return json_response(StatusCode::NOT_FOUND, json!({"error": "not found"}), None);
    };
    if header_str(&parts.headers, "upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket")) {
        if provider != Provider::Codex {
            return provider_error(
                provider,
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "conflux: websockets are only supported for Codex",
            );
        }
        return crate::ws::handle(app.clone(), parts, path).await;
    }
    if provider == Provider::Claude && app.cfg.claude_code_only && !is_claude_code(&parts.headers) {
        return provider_error(
            provider,
            StatusCode::FORBIDDEN,
            "permission_error",
            "conflux: Claude subscription accounts only serve Claude Code",
        );
    }
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return provider_error(
                provider,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "conflux: request body is over 32 MB",
            );
        }
    };
    let route = route_request(provider, &parts.headers, &body);
    let machine = machine_name(&app, &parts.headers);
    let started = Instant::now();
    let log = |account: &AccountEntry, status: u16| {
        (parts.method == http::Method::POST)
            .then(|| log_attempt(&app, provider, account, &route, &machine, status, started))
    };
    let attempt = Attempt {
        app: &app,
        provider,
        method: &parts.method,
        url: upstream_url(&app, provider, &path, parts.uri.query()),
        headers: &parts.headers,
        body: &body,
    };

    let mut tried = HashSet::new();
    let mut last_rejection: Option<Response> = None;
    loop {
        let account = match app.router.pick(&route, &tried) {
            Ok(a) => a,
            Err(why) => return last_rejection.unwrap_or_else(|| no_account_response(provider, why)),
        };
        if !app.accepting_requests() {
            return provider_error(provider, StatusCode::SERVICE_UNAVAILABLE, "api_error", HUB_NOT_ACTIVE);
        }
        tried.insert(account.id.clone());
        let sent_generation = account.credentials().generation;

        let mut resp = match attempt.send(&account).await {
            Ok(r) => r,
            Err(e) if e.is_connect() => {
                tracing::warn!(account = %account.id, "upstream connect failed: {e}");
                last_rejection = Some(provider_error(
                    provider,
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "conflux: could not reach the upstream",
                ));
                continue;
            }
            Err(e) => {
                tracing::warn!(account = %account.id, "upstream request failed after send: {e}");
                return provider_error(
                    provider,
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "conflux: upstream request failed",
                );
            }
        };

        if resp.status() == StatusCode::UNAUTHORIZED {
            match app.refresher.refresh_rejected(&account, sent_generation).await {
                RefreshResult::Refreshed => match attempt.send(&account).await {
                    Ok(r) => resp = r,
                    Err(e) => {
                        tracing::warn!(account = %account.id, "retry after refresh failed: {e}");
                        return provider_error(
                            provider,
                            StatusCode::BAD_GATEWAY,
                            "api_error",
                            "conflux: upstream request failed",
                        );
                    }
                },
                other => {
                    tracing::warn!(account = %account.id, result = ?other, "401 and refresh did not help");
                }
            }
            if resp.status() == StatusCode::UNAUTHORIZED {
                let headers = resp.headers().clone();
                let bytes = read_limited(resp, MAX_REJECTION_BODY).await;
                app.router.cool(
                    &account.id,
                    router::ACCOUNT,
                    Utc::now() + chrono::Duration::seconds(60),
                    "upstream 401",
                );
                log(&account, 401);
                last_rejection = Some(buffered_response(StatusCode::UNAUTHORIZED, &headers, bytes));
                continue;
            }
        }

        app.usage.observe_headers(&account.id, provider, resp.headers());

        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            let headers = resp.headers().clone();
            let bytes = read_limited(resp, MAX_REJECTION_BODY).await;
            let now = Utc::now();
            let rejection = match provider {
                Provider::Claude => classify_claude_429(&headers, route.model.as_deref(), now),
                Provider::Codex => classify_codex_429(&headers, &bytes, route.model.as_deref(), now),
            };
            match &rejection {
                Rejection::Quota { scope, until, reason } => app.router.cool(&account.id, scope, *until, reason),
                Rejection::Short { scope, until } => app.router.cool(&account.id, scope, *until, "short rate limit"),
            }
            log(&account, 429);
            last_rejection = Some(buffered_response(StatusCode::TOO_MANY_REQUESTS, &headers, bytes));
            continue;
        }

        let record = log(&account, resp.status().as_u16());
        let scanner = BodyScanner::new(
            header_str(resp.headers(), "content-encoding"),
            header_str(resp.headers(), "content-type"),
        );
        let tap = ResponseTap::new(
            scanner,
            provider,
            record,
            app.requests.clone(),
            app.router.clone(),
            account.id.clone(),
        );
        return stream_response(resp, tap);
    }
}

fn stream_response(resp: reqwest::Response, mut tap: ResponseTap) -> Response {
    let status = resp.status();
    let headers = forward_response_headers(resp.headers());
    let stream = guarded_stream(resp.bytes_stream(), move |chunk| tap.feed(chunk));
    let mut r = Response::builder().status(status);
    if let Some(h) = r.headers_mut() {
        *h = headers;
    }
    r.body(Body::from_stream(stream)).expect("valid response")
}

fn guarded_stream<S, F>(inner: S, mut on_chunk: F) -> impl Stream<Item = Result<Bytes, std::io::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
    F: FnMut(&[u8]) + Send + 'static,
{
    futures_util::stream::unfold(Some(inner), |state| async move {
        let mut inner = state?;
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, inner.next()).await {
            Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(inner))),
            Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e)), None)),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "upstream idle")),
                None,
            )),
        }
    })
    .map(move |item| {
        if let Ok(chunk) = &item {
            on_chunk(chunk);
        }
        item
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn provider_detection() {
        assert_eq!(provider_for("/v1/messages", "anything"), Some(Provider::Claude));
        assert_eq!(provider_for("/v1/messages/count_tokens", ""), Some(Provider::Claude));
        assert_eq!(
            provider_for("/v1/models", "claude-cli/2.1 (external, cli)"),
            Some(Provider::Claude)
        );
        assert_eq!(provider_for("/v1/models", "codex_cli_rs/0.1"), Some(Provider::Codex));
        assert_eq!(provider_for("/v1/responses", ""), Some(Provider::Codex));
        assert_eq!(provider_for("/backend-api/codex/responses", ""), Some(Provider::Codex));
        assert_eq!(provider_for("/conflux/api/status", ""), None);
    }

    #[test]
    fn header_denylist() {
        let h = hm(&[
            ("authorization", "Bearer client"),
            ("x-api-key", "k"),
            ("cookie", "c"),
            ("x-forwarded-for", "1.2.3.4"),
            ("proxy-authorization", "p"),
            ("anthropic-beta", "a"),
            ("x-codex-routing-hint", "model=x"),
            ("accept-encoding", "gzip"),
        ]);
        let out = forward_request_headers(&h);
        let names: Vec<_> = out.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, vec!["anthropic-beta", "x-codex-routing-hint", "accept-encoding"]);
    }

    #[test]
    fn claude_code_detection() {
        assert!(is_claude_code(&hm(&[
            ("user-agent", "claude-cli/2.1.280 (external, cli)"),
            ("x-app", "cli")
        ])));
        assert!(!is_claude_code(&hm(&[(
            "user-agent",
            "claude-cli/2.1.280 (external, cli)"
        )])));
        assert!(!is_claude_code(&hm(&[("user-agent", "curl/8"), ("x-app", "cli")])));
    }

    #[test]
    fn sessions() {
        let h = hm(&[("x-claude-code-session-id", "s-hdr")]);
        assert_eq!(claude_session(&h, &Value::Null).as_deref(), Some("s-hdr"));
        let body = json!({"metadata": {"user_id": "{\"device_id\":\"d\",\"session_id\":\"s-json\"}"}});
        assert_eq!(claude_session(&HeaderMap::new(), &body).as_deref(), Some("s-json"));
        let body = json!({"metadata": {"user_id": "user_abc_account_def_session_s-legacy"}});
        assert_eq!(claude_session(&HeaderMap::new(), &body).as_deref(), Some("s-legacy"));
        let h = hm(&[("x-codex-turn-metadata", "{\"session_id\":\"cx\"}")]);
        assert_eq!(codex_session(&h, &Value::Null).as_deref(), Some("cx"));
        assert_eq!(
            codex_session(&hm(&[("session_id", "a")]), &Value::Null).as_deref(),
            Some("a")
        );
        assert_eq!(
            codex_session(&HeaderMap::new(), &json!({"prompt_cache_key": "pk"})).as_deref(),
            Some("pk")
        );
    }

    #[test]
    fn claude_429_scopes() {
        let now = Utc::now();
        let shared = hm(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", "1900000000"),
        ]);
        match classify_claude_429(&shared, Some("claude-fable-5-1"), now) {
            Rejection::Quota { scope, until, .. } => {
                assert_eq!(scope, router::ACCOUNT);
                assert_eq!(until.timestamp(), 1_900_000_000);
            }
            r => panic!("{r:?}"),
        }
        let scoped = hm(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ]);
        match classify_claude_429(&scoped, Some("claude-fable-5-1"), now) {
            Rejection::Quota { scope, .. } => assert_eq!(scope, router::model_scope("claude-fable-5-1")),
            r => panic!("{r:?}"),
        }
        let short = hm(&[("retry-after", "12")]);
        match classify_claude_429(&short, Some("m"), now) {
            Rejection::Short { until, .. } => assert_eq!((until - now).num_seconds(), 12),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn codex_error_envelopes() {
        let now = Utc::now();
        for body in [
            json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 100}}),
            json!({"type": "usage_limit_reached", "resets_in_seconds": 100}),
            json!({"type": "error", "body": {"error": {"type": "usage_limit_reached", "resets_in_seconds": 100}}}),
            json!({"type": "response.failed", "response": {"error": {"code": "usage_limit_reached", "resets_in_seconds": 100}}}),
        ] {
            assert!(is_codex_usage_limit(&body), "{body}");
            assert_eq!((codex_reset(&body, now).unwrap() - now).num_seconds(), 100);
        }
        let capacity = json!({"error": {"type": "rate_limit_error"}});
        assert!(matches!(
            classify_codex_429(&HeaderMap::new(), capacity.to_string().as_bytes(), None, now),
            Rejection::Short { .. }
        ));
    }
}

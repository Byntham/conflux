use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::accounts::{AccountEntry, Provider};
use crate::app::App;
use crate::config::Endpoints;
use crate::fsutil;

const TOKEN_PLACEHOLDER: &str = "$TOKEN$";
const MAX_RESPONSE: usize = 1024 * 1024;

pub fn load_or_create_key(file: &Path) -> Result<String> {
    if let Ok(existing) = std::fs::read_to_string(file) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return Ok(existing);
        }
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = file.parent() {
        fsutil::ensure_private_dir(dir)?;
    }
    fsutil::write_durable(file, key.as_bytes())?;
    Ok(key)
}

pub fn api_call_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build api-call client")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn fail(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

pub async fn require_key(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(presented.as_bytes(), app.management_key.as_bytes()) {
        return fail(StatusCode::UNAUTHORIZED, "invalid management key");
    }
    next.run(req).await
}

fn live_account(app: &App, id: &str) -> Option<Arc<AccountEntry>> {
    app.store.get(id).filter(|e| e.credentials().generation > 0)
}

fn without_nulls(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, x)| !x.is_null())
                .map(|(k, x)| (k, without_nulls(x)))
                .collect(),
        ),
        other => other,
    }
}

pub async fn auth_files(State(app): State<Arc<App>>) -> Json<Value> {
    let files: Vec<Value> = app
        .store
        .all()
        .iter()
        .filter(|e| e.credentials().generation > 0)
        .map(|e| {
            let c = e.credentials();
            let mut file = json!({
                "id": e.id,
                "auth_index": e.id,
                "provider": e.provider.as_str(),
                "email": c.email,
                "disabled": !e.settings().enabled,
            });
            if e.provider == Provider::Codex {
                file["id_token"] = json!({
                    "chatgpt_account_id": c.chatgpt_account_id,
                    "chatgpt_plan_type": c.plan,
                    "plan_type": c.plan,
                });
            }
            without_nulls(file)
        })
        .collect();
    Json(json!({ "files": files }))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AllowedCall {
    ClaudeUsage,
    CodexUsage,
    CodexCredits,
    CodexConsumeCredit,
}

impl AllowedCall {
    pub fn parse(method: &str, url: &str) -> Option<Self> {
        match (method.to_ascii_uppercase().as_str(), url) {
            ("GET", "https://api.anthropic.com/api/oauth/usage") => Some(Self::ClaudeUsage),
            ("GET", "https://chatgpt.com/backend-api/wham/usage") => Some(Self::CodexUsage),
            ("GET", "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits") => Some(Self::CodexCredits),
            ("POST", "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume") => {
                Some(Self::CodexConsumeCredit)
            }
            _ => None,
        }
    }

    fn provider(self) -> Provider {
        match self {
            Self::ClaudeUsage => Provider::Claude,
            _ => Provider::Codex,
        }
    }

    fn method(self) -> reqwest::Method {
        match self {
            Self::CodexConsumeCredit => reqwest::Method::POST,
            _ => reqwest::Method::GET,
        }
    }

    fn url(self, ep: &Endpoints) -> String {
        match self {
            Self::ClaudeUsage => format!("{}/api/oauth/usage", ep.anthropic_api),
            Self::CodexUsage => format!("{}/wham/usage", ep.chatgpt_backend),
            Self::CodexCredits => format!("{}/wham/rate-limit-reset-credits", ep.chatgpt_backend),
            Self::CodexConsumeCredit => format!("{}/wham/rate-limit-reset-credits/consume", ep.chatgpt_backend),
        }
    }
}

#[derive(Deserialize)]
pub struct ApiCall {
    auth_index: String,
    method: String,
    url: String,
    #[serde(default)]
    header: BTreeMap<String, String>,
    #[serde(default)]
    data: Option<String>,
}

async fn read_capped(resp: reqwest::Response) -> Result<String, reqwest::Error> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let room = MAX_RESPONSE.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= MAX_RESPONSE {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).to_string())
}

pub async fn api_call(State(app): State<Arc<App>>, Json(call): Json<ApiCall>) -> Response {
    let Some(allowed) = AllowedCall::parse(&call.method, &call.url) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "this hub only allows the usage and reset-credit calls T3 Code makes",
        );
    };
    let Some(account) = live_account(&app, &call.auth_index) else {
        return fail(StatusCode::NOT_FOUND, "account not found");
    };
    if account.provider != allowed.provider() {
        return fail(
            StatusCode::BAD_REQUEST,
            "this call does not match the account's provider",
        );
    }
    let token = account.access_token();
    let mut req = app.api_call_http.request(allowed.method(), allowed.url(&app.ep));
    for (k, v) in &call.header {
        req = req.header(k.as_str(), v.replace(TOKEN_PLACEHOLDER, &token));
    }
    if let Some(data) = call.data {
        req = req.body(data.replace(TOKEN_PLACEHOLDER, &token));
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(account = %account.id, "api-call failed: {e}");
            return fail(StatusCode::BAD_GATEWAY, "the provider could not be reached");
        }
    };
    let status_code = resp.status().as_u16();
    let header: BTreeMap<String, Vec<String>> = resp
        .headers()
        .keys()
        .map(|k| {
            let values = resp
                .headers()
                .get_all(k)
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_string))
                .collect();
            (k.to_string(), values)
        })
        .collect();
    match read_capped(resp).await {
        Ok(body) => Json(json!({ "status_code": status_code, "header": header, "body": body })).into_response(),
        Err(_) => fail(StatusCode::BAD_GATEWAY, "the provider response could not be read"),
    }
}

#[derive(Deserialize)]
pub struct ResetQuota {
    auth_index: String,
}

pub async fn reset_quota(State(app): State<Arc<App>>, Json(req): Json<ResetQuota>) -> Response {
    let Some(account) = live_account(&app, &req.auth_index) else {
        return fail(StatusCode::NOT_FOUND, "account not found");
    };
    app.router.clear(&account.id);
    app.usage.invalidate(&account.id);
    let usage = app.usage.clone();
    tokio::spawn(async move {
        if let Err(e) = usage.poll(&account).await {
            tracing::warn!(account = %account.id, "usage poll after reset failed: {e}");
        }
    });
    Json(json!({ "status": "ok", "auth_index": req.auth_index })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_calls_are_allowed() {
        assert_eq!(
            AllowedCall::parse("get", "https://api.anthropic.com/api/oauth/usage"),
            Some(AllowedCall::ClaudeUsage)
        );
        assert_eq!(
            AllowedCall::parse(
                "POST",
                "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"
            ),
            Some(AllowedCall::CodexConsumeCredit)
        );
        assert_eq!(
            AllowedCall::parse("POST", "https://api.anthropic.com/api/oauth/usage"),
            None
        );
        assert_eq!(AllowedCall::parse("GET", "https://api.anthropic.com/v1/messages"), None);
        assert_eq!(AllowedCall::parse("GET", "https://evil.example/wham/usage"), None);
    }
}

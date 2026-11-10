use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::Provider;
use crate::config::Endpoints;

pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_REDIRECT_URI: &str = "http://localhost:54545/callback";
pub const CLAUDE_AUTHORIZE_SCOPE: &str =
    "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub const CLAUDE_REFRESH_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";
pub const CODEX_DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

pub fn control_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("build reqwest client")
}

fn random_b64(len: usize) -> String {
    let mut buf = vec![0u8; len];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

#[derive(Clone, Debug)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = random_b64(96);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

pub fn new_state() -> String {
    random_b64(32)
}

pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodexIdentity {
    pub chatgpt_account_id: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
}

pub fn codex_identity(id_token: &str) -> CodexIdentity {
    let Some(claims) = jwt_claims(id_token) else {
        return CodexIdentity::default();
    };
    let auth = &claims["https://api.openai.com/auth"];
    let s = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    CodexIdentity {
        chatgpt_account_id: s(&auth["chatgpt_account_id"]),
        email: s(&claims["email"]),
        plan_type: s(&auth["chatgpt_plan_type"]),
    }
}

pub fn claude_authorize_url(ep: &Endpoints, pkce: &Pkce, state: &str) -> String {
    let mut url = url::Url::parse(&ep.claude_authorize).expect("valid authorize url");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLAUDE_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", CLAUDE_REDIRECT_URI)
        .append_pair("scope", CLAUDE_AUTHORIZE_SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

pub fn parse_claude_code(pasted: &str) -> (String, Option<String>) {
    let pasted = pasted.trim();
    if let Ok(u) = url::Url::parse(pasted) {
        let mut code = None;
        let mut state = None;
        for (k, v) in u.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                _ => {}
            }
        }
        if let Some(c) = code {
            return (c, state);
        }
    }
    match pasted.split_once('#') {
        Some((c, s)) => (c.to_string(), Some(s.to_string())),
        None => (pasted.to_string(), None),
    }
}

#[derive(Clone, Debug)]
pub struct LoginTokens {
    pub provider: Provider,
    pub identity: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub chatgpt_account_id: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub id_token: Option<String>,
}

fn claude_headers(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("User-Agent", "axios/1.15.2")
}

pub async fn claude_exchange(
    http: &reqwest::Client,
    ep: &Endpoints,
    pasted: &str,
    pkce: &Pkce,
    issued_state: &str,
) -> Result<LoginTokens> {
    let (code, state) = parse_claude_code(pasted);
    let state = state.unwrap_or_else(|| issued_state.to_string());
    if state != issued_state {
        bail!("the pasted code belongs to a different login attempt (state mismatch)");
    }
    let body = format!(
        r#"{{"grant_type":"authorization_code","code":{},"redirect_uri":{},"client_id":{},"code_verifier":{},"state":{}}}"#,
        json!(code),
        json!(CLAUDE_REDIRECT_URI),
        json!(CLAUDE_CLIENT_ID),
        json!(pkce.verifier),
        json!(state)
    );
    let resp = claude_headers(http.post(&ep.claude_token)).body(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("Claude token exchange failed (HTTP {status}): {}", truncate(&text, 300));
    }
    let v: Value = serde_json::from_str(&text).context("parse Claude token response")?;
    let access = v["access_token"].as_str().context("no access_token")?.to_string();
    let refresh = v["refresh_token"].as_str().context("no refresh_token")?.to_string();
    let expires_in = v["expires_in"].as_i64().unwrap_or(3600);
    let account_uuid = v["account"]["uuid"].as_str().unwrap_or_default();
    let org_uuid = v["organization"]["uuid"].as_str().unwrap_or_default();
    if account_uuid.is_empty() {
        bail!("Claude token response has no account uuid");
    }
    Ok(LoginTokens {
        provider: Provider::Claude,
        identity: format!("{account_uuid}:{org_uuid}"),
        email: v["account"]["email_address"].as_str().map(str::to_string),
        plan: None,
        chatgpt_account_id: None,
        access_token: access,
        refresh_token: refresh,
        expires_at: Utc::now() + chrono::Duration::seconds(expires_in),
        id_token: None,
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct DeviceStart {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval_secs: u64,
}

pub async fn codex_device_start(http: &reqwest::Client, ep: &Endpoints) -> Result<DeviceStart> {
    let url = format!("{}/api/accounts/deviceauth/usercode", ep.openai_auth);
    let resp = http
        .post(url)
        .header("Accept", "application/json")
        .json(&json!({ "client_id": CODEX_CLIENT_ID }))
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!(
            "Codex device code request failed (HTTP {status}): {}",
            truncate(&text, 300)
        );
    }
    let v: Value = serde_json::from_str(&text)?;
    let user_code = v["user_code"]
        .as_str()
        .or_else(|| v["usercode"].as_str())
        .context("no user_code")?;
    let interval = match &v["interval"] {
        Value::Number(n) => n.as_u64().unwrap_or(5),
        Value::String(s) => s.parse().unwrap_or(5),
        _ => 5,
    };
    Ok(DeviceStart {
        device_auth_id: v["device_auth_id"].as_str().context("no device_auth_id")?.to_string(),
        user_code: user_code.to_string(),
        interval_secs: interval.max(1),
    })
}

pub enum DevicePoll {
    Pending,
    Done {
        authorization_code: String,
        code_verifier: String,
    },
}

pub async fn codex_device_poll(http: &reqwest::Client, ep: &Endpoints, start: &DeviceStart) -> Result<DevicePoll> {
    let url = format!("{}/api/accounts/deviceauth/token", ep.openai_auth);
    let resp = http
        .post(url)
        .header("Accept", "application/json")
        .json(&json!({ "device_auth_id": start.device_auth_id, "user_code": start.user_code }))
        .send()
        .await?;
    let status = resp.status().as_u16();
    if status == 403 || status == 404 {
        return Ok(DevicePoll::Pending);
    }
    let text = resp.text().await?;
    if !(200..300).contains(&status) {
        bail!("Codex device poll failed (HTTP {status}): {}", truncate(&text, 300));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(DevicePoll::Done {
        authorization_code: v["authorization_code"]
            .as_str()
            .context("no authorization_code")?
            .to_string(),
        code_verifier: v["code_verifier"].as_str().context("no code_verifier")?.to_string(),
    })
}

pub async fn codex_exchange(
    http: &reqwest::Client,
    ep: &Endpoints,
    authorization_code: &str,
    code_verifier: &str,
) -> Result<LoginTokens> {
    let url = format!("{}/oauth/token", ep.openai_auth);
    let resp = http
        .post(url)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT_ID),
            ("code", authorization_code),
            ("redirect_uri", CODEX_DEVICE_REDIRECT_URI),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("Codex token exchange failed (HTTP {status}): {}", truncate(&text, 300));
    }
    let v: Value = serde_json::from_str(&text)?;
    let id_token = v["id_token"].as_str().context("no id_token")?.to_string();
    let ident = codex_identity(&id_token);
    let account_id = ident
        .chatgpt_account_id
        .clone()
        .ok_or_else(|| anyhow!("id_token has no chatgpt_account_id"))?;
    Ok(LoginTokens {
        provider: Provider::Codex,
        identity: account_id.clone(),
        email: ident.email,
        plan: ident.plan_type,
        chatgpt_account_id: Some(account_id),
        access_token: v["access_token"].as_str().context("no access_token")?.to_string(),
        refresh_token: v["refresh_token"].as_str().context("no refresh_token")?.to_string(),
        expires_at: Utc::now() + chrono::Duration::seconds(v["expires_in"].as_i64().unwrap_or(864_000)),
        id_token: Some(id_token),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
    pub id_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RefreshOutcome {
    Ok(RefreshedTokens),
    Rejected(String),
    RetryAfter(Duration),
    NotSent(String),
    Unknown(String),
}

const TERMINAL_MARKERS: &[&str] = &[
    "invalid_grant",
    "refresh_token_reused",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "token_revoked",
    "invalid_client",
];

pub async fn refresh(
    http: &reqwest::Client,
    ep: &Endpoints,
    provider: Provider,
    refresh_token: &str,
) -> RefreshOutcome {
    let req = match provider {
        Provider::Claude => {
            let body = json!({
                "client_id": CLAUDE_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "scope": CLAUDE_REFRESH_SCOPE,
            });
            claude_headers(http.post(&ep.claude_token)).body(body.to_string())
        }
        Provider::Codex => http
            .post(format!("{}/oauth/token", ep.openai_auth))
            .header("Accept", "application/json")
            .form(&[
                ("client_id", CODEX_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("scope", "openid profile email"),
            ]),
    };
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => return RefreshOutcome::NotSent(e.to_string()),
        Err(e) => return RefreshOutcome::Unknown(format!("transport: {e}")),
    };
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return RefreshOutcome::Unknown(format!("read body: {e}")),
    };
    classify_refresh(provider, status, retry_after, &text)
}

pub fn classify_refresh(provider: Provider, status: u16, retry_after: Option<u64>, body: &str) -> RefreshOutcome {
    if status == 200 {
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            return RefreshOutcome::Unknown("undecodable 200 body".into());
        };
        let access = v["access_token"].as_str().filter(|s| !s.trim().is_empty());
        let expires_in = v["expires_in"].as_i64().filter(|n| *n > 0);
        let refresh = v["refresh_token"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        let (Some(access), Some(expires_in)) = (access, expires_in) else {
            return RefreshOutcome::Unknown("200 body missing access_token or expires_in".into());
        };
        if provider == Provider::Codex && refresh.is_none() {
            return RefreshOutcome::Unknown("Codex 200 body has no refresh_token".into());
        }
        return RefreshOutcome::Ok(RefreshedTokens {
            access_token: access.to_string(),
            refresh_token: refresh,
            expires_in,
            id_token: v["id_token"].as_str().map(str::to_string),
        });
    }
    let lower = body.to_lowercase();
    if (status == 400 || status == 401) && TERMINAL_MARKERS.iter().any(|m| lower.contains(m)) {
        return RefreshOutcome::Rejected(truncate(body, 200));
    }
    if provider == Provider::Codex && status == 401 {
        return RefreshOutcome::Rejected(truncate(body, 200));
    }
    if provider == Provider::Claude && status == 429 {
        return RefreshOutcome::RetryAfter(Duration::from_secs(retry_after.unwrap_or(30)));
    }
    RefreshOutcome::Unknown(format!("HTTP {status}: {}", truncate(body, 200)))
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pasted_codes() {
        assert_eq!(parse_claude_code("abc#xyz"), ("abc".into(), Some("xyz".into())));
        assert_eq!(parse_claude_code(" abc "), ("abc".into(), None));
        assert_eq!(
            parse_claude_code("http://localhost:54545/callback?code=c1&state=s1"),
            ("c1".into(), Some("s1".into()))
        );
    }

    #[test]
    fn codex_identity_reads_nested_claim() {
        let payload = json!({
            "email": "me@example.com",
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1", "chatgpt_plan_type": "pro"}
        });
        let tok = format!("x.{}.y", URL_SAFE_NO_PAD.encode(payload.to_string()));
        let id = codex_identity(&tok);
        assert_eq!(id.chatgpt_account_id.as_deref(), Some("acct-1"));
        assert_eq!(id.email.as_deref(), Some("me@example.com"));
        assert_eq!(id.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn refresh_classification() {
        use Provider::*;
        let ok = r#"{"access_token":"a","refresh_token":"r","expires_in":3600}"#;
        assert!(matches!(classify_refresh(Claude, 200, None, ok), RefreshOutcome::Ok(_)));
        let no_rt = r#"{"access_token":"a","expires_in":3600}"#;
        match classify_refresh(Claude, 200, None, no_rt) {
            RefreshOutcome::Ok(t) => assert_eq!(t.refresh_token, None),
            o => panic!("{o:?}"),
        }
        assert!(matches!(
            classify_refresh(Codex, 200, None, no_rt),
            RefreshOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_refresh(Claude, 200, None, "nope"),
            RefreshOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_refresh(Claude, 400, None, r#"{"error":"invalid_grant"}"#),
            RefreshOutcome::Rejected(_)
        ));
        assert!(matches!(
            classify_refresh(Codex, 401, None, "{}"),
            RefreshOutcome::Rejected(_)
        ));
        assert_eq!(
            classify_refresh(Claude, 429, Some(7), ""),
            RefreshOutcome::RetryAfter(Duration::from_secs(7))
        );
        assert!(matches!(
            classify_refresh(Codex, 429, None, ""),
            RefreshOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_refresh(Claude, 503, None, "busy"),
            RefreshOutcome::Unknown(_)
        ));
        assert!(matches!(
            classify_refresh(Claude, 400, None, "bad request"),
            RefreshOutcome::Unknown(_)
        ));
    }
}

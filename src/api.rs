use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{AccountEntry, RefreshState};
use crate::app::App;
use crate::hubstate::HubState;
use crate::oauth::{self, DevicePoll, Pkce};
use crate::update;

const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);

pub enum PendingLogin {
    Claude {
        pkce: Pkce,
        state: String,
        created: Instant,
    },
    Codex {
        status: Arc<Mutex<LoginStatus>>,
        created: Instant,
    },
}

impl PendingLogin {
    fn created(&self) -> Instant {
        match self {
            PendingLogin::Claude { created, .. } | PendingLogin::Codex { created, .. } => *created,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum LoginStatus {
    Pending { user_code: String, verify_url: String },
    Done { account: String, email: Option<String> },
    Failed { error: String },
}

#[derive(Default)]
pub struct Logins {
    map: Mutex<HashMap<String, PendingLogin>>,
}

impl Logins {
    fn insert(&self, p: PendingLogin) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut m = self.map.lock().unwrap();
        m.retain(|_, v| v.created().elapsed() < LOGIN_TTL);
        m.insert(id.clone(), p);
        id
    }
}

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

#[derive(Serialize)]
pub struct AccountView {
    pub id: String,
    pub provider: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub enabled: bool,
    pub allow_overage: bool,
    pub needs_login: Option<String>,
    pub refresh: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub billing_blocked: Option<String>,
    pub cooldowns: Vec<crate::router::Cooldown>,
    pub usage: crate::usage::Snapshot,
}

pub fn account_view(app: &App, e: &AccountEntry) -> AccountView {
    let c = e.credentials();
    let s = e.settings();
    AccountView {
        id: e.id.clone(),
        provider: e.provider.to_string(),
        email: c.email,
        plan: c.plan,
        enabled: s.enabled,
        allow_overage: s.allow_overage,
        needs_login: e.needs_login(),
        refresh: match e.refresh_state() {
            RefreshState::Normal => "ok".into(),
            RefreshState::RetryAt { at, .. } => format!("retry at {at}"),
            RefreshState::Unknown => "unknown".into(),
            RefreshState::Rejected(r) => format!("rejected: {r}"),
        },
        expires_at: c.grant.expires_at,
        billing_blocked: app.router.billing_blocked(&e.id),
        cooldowns: app.router.cooldowns(&e.id),
        usage: app.usage.get(&e.id),
    }
}

pub async fn status(State(app): State<Arc<App>>) -> Json<Value> {
    let accounts: Vec<_> = app
        .store
        .all()
        .iter()
        .filter(|e| e.credentials().generation > 0)
        .map(|e| account_view(&app, e))
        .collect();
    Json(json!({
        "role": app.cfg.role,
        "version": update::CURRENT,
        "pid": std::process::id(),
        "accounts": accounts,
    }))
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    #[serde(default)]
    machines: Option<Vec<String>>,
}

pub async fn update(State(app): State<Arc<App>>, Json(req): Json<UpdateRequest>) -> ApiResult<Json<Value>> {
    let version = app.updater.check_latest().await.map_err(|e| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            format!("could not check for a new release: {e:#}"),
        )
    })?;
    let mut started = Vec::new();
    let mut skipped = Vec::new();
    let mut hub_starts = false;
    for m in app.fleet() {
        if req.machines.as_ref().is_some_and(|only| !only.contains(&m.hostname)) {
            continue;
        }
        let is_hub = m.role == "hub";
        let obstacle = if !m.outdated {
            Some("already up to date")
        } else if !m.online {
            Some("offline")
        } else if m.update.as_ref().is_some_and(update::UpdateStatus::in_progress) {
            Some("already updating")
        } else if is_hub && app.hub_state.get() != HubState::Active {
            Some("the hub is moving")
        } else if !is_hub
            && !app
                .machines
                .send(&m.hostname, json!({ "type": "update", "version": version }))
        {
            Some("offline")
        } else {
            None
        };
        match obstacle {
            Some(reason) => skipped.push(json!({ "machine": m.hostname, "reason": reason })),
            None => {
                hub_starts |= is_hub;
                started.push(m.hostname);
            }
        }
    }
    let known: Vec<String> = app.fleet().into_iter().map(|m| m.hostname).collect();
    for name in req.machines.iter().flatten().filter(|name| !known.contains(name)) {
        skipped.push(json!({ "machine": name, "reason": "unknown machine" }));
    }
    if hub_starts {
        tokio::spawn(
            app.updater
                .clone()
                .run(version.clone(), app.lifecycle.clone(), app.clone()),
        );
    }
    Ok(Json(
        json!({ "version": version, "started": started, "skipped": skipped }),
    ))
}

pub async fn claude_start(State(app): State<Arc<App>>) -> Json<Value> {
    let pkce = Pkce::new();
    let state = oauth::new_state();
    let url = oauth::claude_authorize_url(&app.ep, &pkce, &state);
    let id = app.logins.insert(PendingLogin::Claude {
        pkce,
        state,
        created: Instant::now(),
    });
    Json(json!({ "login_id": id, "url": url }))
}

#[derive(Deserialize)]
pub struct ClaudeFinish {
    login_id: String,
    code: String,
}

pub async fn claude_finish(State(app): State<Arc<App>>, Json(req): Json<ClaudeFinish>) -> ApiResult<Json<Value>> {
    let pending = app.logins.map.lock().unwrap().remove(&req.login_id);
    let Some(PendingLogin::Claude { pkce, state, .. }) = pending else {
        return Err(not_found("login"));
    };
    let tokens = oauth::claude_exchange(&app.control_http, &app.ep, &req.code, &pkce, &state).await?;
    let e = app.refresher.install_login(tokens).await?;
    Ok(Json(json!({ "account": e.id, "email": e.credentials().email })))
}

pub async fn codex_start(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let start = oauth::codex_device_start(&app.control_http, &app.ep).await?;
    let pending = LoginStatus::Pending {
        user_code: start.user_code.clone(),
        verify_url: oauth::CODEX_DEVICE_VERIFY_URL.to_string(),
    };
    let status = Arc::new(Mutex::new(pending.clone()));
    let id = app.logins.insert(PendingLogin::Codex {
        status: status.clone(),
        created: Instant::now(),
    });
    tokio::spawn(poll_codex_login(app.clone(), start, status));
    Ok(Json(json!({ "login_id": id, "status": pending })))
}

async fn poll_codex_login(app: Arc<App>, start: oauth::DeviceStart, status: Arc<Mutex<LoginStatus>>) {
    let deadline = Instant::now() + LOGIN_TTL;
    let outcome = loop {
        if Instant::now() > deadline {
            break Err(anyhow::anyhow!("timed out waiting for the device code to be entered"));
        }
        tokio::time::sleep(Duration::from_secs(start.interval_secs)).await;
        match oauth::codex_device_poll(&app.control_http, &app.ep, &start).await {
            Ok(DevicePoll::Pending) => continue,
            Ok(DevicePoll::Done {
                authorization_code,
                code_verifier,
            }) => {
                break async {
                    let tokens =
                        oauth::codex_exchange(&app.control_http, &app.ep, &authorization_code, &code_verifier).await?;
                    app.refresher.install_login(tokens).await
                }
                .await;
            }
            Err(e) => break Err(e),
        }
    };
    *status.lock().unwrap() = match outcome {
        Ok(e) => LoginStatus::Done {
            account: e.id.clone(),
            email: e.credentials().email,
        },
        Err(e) => LoginStatus::Failed {
            error: format!("{e:#}"),
        },
    };
}

pub async fn login_status(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<LoginStatus>> {
    let m = app.logins.map.lock().unwrap();
    match m.get(&id) {
        Some(PendingLogin::Codex { status, .. }) => Ok(Json(status.lock().unwrap().clone())),
        _ => Err(not_found("login")),
    }
}

fn account(app: &App, id: &str) -> ApiResult<Arc<AccountEntry>> {
    app.store
        .get(id)
        .filter(|e| e.credentials().generation > 0)
        .ok_or_else(|| not_found("account"))
}

#[derive(Deserialize)]
pub struct SettingsPatch {
    enabled: Option<bool>,
    allow_overage: Option<bool>,
}

pub async fn update_account(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(patch): Json<SettingsPatch>,
) -> ApiResult<Json<AccountView>> {
    let e = account(&app, &id)?;
    e.update_settings(|s| {
        if let Some(v) = patch.enabled {
            s.enabled = v;
        }
        if let Some(v) = patch.allow_overage {
            s.allow_overage = v;
        }
    })?;
    Ok(Json(account_view(&app, &e)))
}

pub async fn remove_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let e = account(&app, &id)?;
    let _op = e.op_lock.lock().await;
    app.store.remove(&id)?;
    app.router.clear(&id);
    Ok(Json(json!({ "removed": id })))
}

pub async fn poll_usage(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<AccountView>> {
    let e = account(&app, &id)?;
    app.usage.poll(&e).await?;
    Ok(Json(account_view(&app, &e)))
}

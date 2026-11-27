use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{AccountEntry, RefreshState};
use crate::app::App;
use crate::guests::{Access, Guest};
use crate::hubstate::HubState;
use crate::machines;
use crate::oauth::{self, DevicePoll, DeviceToken, Pkce};
use crate::{dashboard, setup, update};

const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);

pub enum PendingLogin {
    Claude {
        pkce: Pkce,
        state: String,
        created: Instant,
    },
    Device {
        status: Arc<Mutex<LoginStatus>>,
        created: Instant,
    },
}

impl PendingLogin {
    fn created(&self) -> Instant {
        match self {
            PendingLogin::Claude { created, .. } | PendingLogin::Device { created, .. } => *created,
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
    pub cache_hit: Option<u64>,
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
        cache_hit: app.requests.cache_hit(&e.id),
    }
}

pub async fn status(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<Value> {
    if !app.identify(from.ip()).await.is_owner() {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        return Json(json!({
            "role": app.cfg.role,
            "version": update::CURRENT,
            "hub": dashboard::hub_name(&app, host),
        }));
    }
    let accounts: Vec<_> = app
        .store
        .all()
        .iter()
        .filter(|e| e.credentials().generation > 0)
        .map(|e| account_view(&app, e))
        .collect();
    let waiting: Vec<_> = app
        .fleet()
        .into_iter()
        .filter(|m| m.access == Some(Access::Pending))
        .map(|m| json!({ "id": m.id, "hostname": m.hostname, "user": m.user }))
        .collect();
    Json(json!({
        "role": app.cfg.role,
        "version": update::CURRENT,
        "pid": std::process::id(),
        "accounts": accounts,
        "guests_waiting": waiting,
    }))
}

#[derive(Deserialize)]
pub struct UpdateRequest {
    #[serde(default)]
    pub machines: Option<Vec<String>>,
}

pub async fn latest_release(updater: &update::Updater) -> ApiResult<String> {
    updater.check_latest().await.map_err(|e| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            format!("could not check for a new release: {e:#}"),
        )
    })
}

pub async fn check_release(updater: &update::Updater) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "latest_version": latest_release(updater).await? })))
}

pub async fn check_update(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    check_release(&app.updater).await
}

pub async fn update(State(app): State<Arc<App>>, Json(req): Json<UpdateRequest>) -> ApiResult<Json<Value>> {
    let version = latest_release(&app.updater).await?;
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
        } else if m.guest {
            Some("guest machine")
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

#[derive(Deserialize)]
pub struct MachinePatch {
    routing: bool,
}

const ROUTING_CONFIRM_LIMIT: Duration = Duration::from_secs(5);

pub async fn update_machine(
    State(app): State<Arc<App>>,
    Path(hostname): Path<String>,
    Json(patch): Json<MachinePatch>,
) -> ApiResult<Json<Value>> {
    let on = patch.routing;
    let done = json!({ "machine": hostname, "routing": on });
    if hostname == app.hostname {
        let hub = app.clone();
        tokio::task::spawn_blocking(move || setup::set_routing(&hub.clients, &hub.paths.config_dir, hub.cfg.port, on))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        return Ok(Json(done));
    }
    let named: Vec<_> = app
        .machines
        .list()
        .into_iter()
        .filter(|m| m.hostname == hostname)
        .collect();
    let machine = named
        .iter()
        .find(|m| !m.guest)
        .or(named.first())
        .ok_or_else(|| not_found("machine"))?;
    if machine.guest {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            format!("{hostname} is a guest machine. Its owner controls its routing."),
        ));
    }
    let offline = || ApiError(StatusCode::CONFLICT, format!("{hostname} is offline"));
    if !machine.online {
        return Err(offline());
    }
    if machine.routing.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("{hostname} runs an older conflux. Update it to change its routing from here."),
        ));
    }
    if !app.machines.send(&hostname, machines::set_routing(on)) {
        return Err(offline());
    }
    if !app
        .machines
        .wait_for(&hostname, ROUTING_CONFIRM_LIMIT, |m| m.routing == Some(on))
        .await
    {
        return Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "{hostname} did not confirm the change. Update it if it runs an older conflux, or check its conflux log."
            ),
        ));
    }
    Ok(Json(done))
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
    Ok(Json(start_device_login(
        &app,
        pending,
        start.interval_secs,
        DeviceFlow::Codex(start),
    )))
}

pub async fn grok_start(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let code = oauth::grok_device_start(&app.control_http, &app.ep).await?;
    let pending = LoginStatus::Pending {
        user_code: code.user_code.clone(),
        verify_url: code.verify_url.clone(),
    };
    Ok(Json(start_device_login(
        &app,
        pending,
        code.interval_secs,
        DeviceFlow::Grok(code),
    )))
}

pub async fn muse_start(State(app): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let code = oauth::muse_device_start(&app.control_http, &app.ep).await?;
    let pending = LoginStatus::Pending {
        user_code: code.user_code.clone(),
        verify_url: code.verify_url.clone(),
    };
    Ok(Json(start_device_login(
        &app,
        pending,
        code.interval_secs,
        DeviceFlow::Muse(code),
    )))
}

enum DeviceFlow {
    Codex(oauth::DeviceStart),
    Grok(oauth::DeviceCode),
    Muse(oauth::DeviceCode),
}

enum DeviceStep {
    Pending,
    SlowDown,
    Done(oauth::LoginTokens),
}

impl DeviceFlow {
    async fn poll(&self, app: &App) -> anyhow::Result<DeviceStep> {
        let (http, ep) = (&app.control_http, &app.ep);
        match self {
            DeviceFlow::Codex(start) => match oauth::codex_device_poll(http, ep, start).await? {
                DevicePoll::Pending => Ok(DeviceStep::Pending),
                DevicePoll::Done {
                    authorization_code,
                    code_verifier,
                } => Ok(DeviceStep::Done(
                    oauth::codex_exchange(http, ep, &authorization_code, &code_verifier).await?,
                )),
            },
            DeviceFlow::Grok(code) => match oauth::grok_device_poll(http, ep, &code.device_code).await? {
                DeviceToken::Pending => Ok(DeviceStep::Pending),
                DeviceToken::SlowDown => Ok(DeviceStep::SlowDown),
                DeviceToken::Done(v) => Ok(DeviceStep::Done(oauth::grok_tokens(http, ep, &v).await?)),
            },
            DeviceFlow::Muse(code) => match oauth::muse_device_poll(http, ep, &code.device_code).await? {
                DeviceToken::Pending => Ok(DeviceStep::Pending),
                DeviceToken::SlowDown => Ok(DeviceStep::SlowDown),
                DeviceToken::Done(v) => Ok(DeviceStep::Done(oauth::muse_tokens(http, ep, &v).await?)),
            },
        }
    }
}

fn start_device_login(app: &Arc<App>, pending: LoginStatus, interval_secs: u64, flow: DeviceFlow) -> Value {
    let status = Arc::new(Mutex::new(pending.clone()));
    let id = app.logins.insert(PendingLogin::Device {
        status: status.clone(),
        created: Instant::now(),
    });
    tokio::spawn(poll_device_login(app.clone(), flow, interval_secs, status));
    json!({ "login_id": id, "status": pending })
}

async fn poll_device_login(app: Arc<App>, flow: DeviceFlow, mut interval_secs: u64, status: Arc<Mutex<LoginStatus>>) {
    let deadline = Instant::now() + LOGIN_TTL;
    let outcome = loop {
        if Instant::now() > deadline {
            break Err(anyhow::anyhow!("timed out waiting for the device code to be entered"));
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        match flow.poll(&app).await {
            Ok(DeviceStep::Pending) => continue,
            Ok(DeviceStep::SlowDown) => interval_secs += 5,
            Ok(DeviceStep::Done(tokens)) => break app.refresher.install_login(tokens).await,
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
        Some(PendingLogin::Device { status, .. }) => Ok(Json(status.lock().unwrap().clone())),
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

fn find_guest(app: &App, name: &str) -> ApiResult<(String, Guest)> {
    let mut known: Vec<(String, Guest)> = app
        .machines
        .list()
        .into_iter()
        .filter(|m| m.guest)
        .filter_map(|m| {
            let guest = Guest {
                login: m.user.unwrap_or_default(),
                hostname: m.hostname,
                approved: false,
                ask: m.ask,
            };
            Some((m.id?, guest))
        })
        .collect();
    for (id, guest) in app.guests.export() {
        if !known.iter().any(|(k, _)| *k == id) {
            known.push((id, guest));
        }
    }
    if let Some(found) = known.iter().find(|(id, _)| id == name) {
        return Ok(found.clone());
    }
    let mut named: Vec<_> = known.into_iter().filter(|(_, g)| g.hostname == name).collect();
    match named.len() {
        0 => Err(not_found("guest machine")),
        1 => Ok(named.remove(0)),
        _ => Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "more than one guest machine is named {name}. Use its ID instead: {}",
                named.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

#[derive(Deserialize)]
pub struct GuestDecision {
    approve: bool,
}

fn set_guest(app: &App, name: &str, approved: bool) -> ApiResult<Json<Value>> {
    let (id, guest) = find_guest(app, name)?;
    let hostname = guest.hostname.clone();
    app.guests
        .set(&id, Guest { approved, ..guest })
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Json(json!({ "id": id, "hostname": hostname, "approved": approved })))
}

pub async fn decide_guest(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(req): Json<GuestDecision>,
) -> ApiResult<Json<Value>> {
    set_guest(&app, &name, req.approve)
}

pub async fn remove_guest(State(app): State<Arc<App>>, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    set_guest(&app, &name, false)
}

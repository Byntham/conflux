use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::App;
use crate::config::{Config, Paths, Role};
use crate::guests::{self, GuestList};
use crate::hubstate::HubState;
use crate::{fsutil, proxy, update};

const MOVE_TIMEOUT: Duration = Duration::from_secs(120);
const COMMIT_ATTEMPTS: u32 = 30;
const RETRY_EVERY: Duration = Duration::from_secs(2);

fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub fn retired_dir(paths: &Paths, id: &str) -> PathBuf {
    paths.data_dir.join(format!("retired-{id}"))
}

pub fn hub_state_file(paths: &Paths) -> PathBuf {
    paths.config_dir.join("hub-state.json")
}

pub fn finish_retirement(paths: &Paths, id: &str) -> Result<()> {
    let accounts = paths.accounts_dir();
    let retired = retired_dir(paths, id);
    if accounts.exists() && !retired.exists() {
        std::fs::rename(&accounts, &retired).context("retire the account store")?;
        fsutil::sync_dir(&paths.data_dir)?;
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct BundleFile {
    pub path: String,
    pub content: String,
}

#[derive(Serialize, Deserialize)]
pub struct Bundle {
    pub id: String,
    pub files: Vec<BundleFile>,
    pub management_key: String,
    #[serde(default)]
    pub guests: GuestList,
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<BundleFile>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if !entry.file_name().to_string_lossy().starts_with('.') {
            let rel = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            out.push(BundleFile {
                path: rel,
                content: STANDARD.encode(std::fs::read(&path)?),
            });
        }
    }
    Ok(())
}

pub fn export_accounts(accounts_dir: &Path) -> Result<Vec<BundleFile>> {
    let mut files = Vec::new();
    collect_files(accounts_dir, accounts_dir, &mut files)?;
    Ok(files)
}

async fn restore_service(app: &App, id: &str) -> Result<bool> {
    let moving = app.hub_state.get();
    if !matches!(&moving, HubState::Moving { id: current, .. } if current == id) {
        return Ok(false);
    }
    let aborted = HubState::Aborted { id: id.to_string() };
    if !app.hub_state.compare_and_set(&moving, aborted.clone())? {
        return Ok(false);
    }
    app.refresher.open_gate().await;
    app.usage.set_paused(false);
    let _ = app.ws_shutdown.send(false);
    app.hub_state.compare_and_set(&aborted, HubState::Active)?;
    Ok(true)
}

pub async fn resume_after(app: Arc<App>, id: String, wait: Duration) {
    tokio::time::sleep(wait).await;
    match restore_service(&app, &id).await {
        Ok(true) => tracing::warn!("the hub move did not finish in time; the hub is back in service"),
        Ok(false) => {}
        Err(e) => tracing::error!("could not put the hub back in service: {e:#}"),
    }
}

struct Target {
    link: u64,
    hub: String,
    address: IpAddr,
}

#[derive(Deserialize)]
pub struct MoveRequest {
    machine: String,
    #[serde(default)]
    force: bool,
}

fn check_move(app: &App, req: &MoveRequest) -> Result<Target, (StatusCode, String)> {
    let refuse = |message: String| Err((StatusCode::CONFLICT, message));
    if app.hub_state.get() != HubState::Active {
        return refuse("the hub is not active".into());
    }
    if app.lifecycle.is_draining() || app.updater.status().is_some_and(|s| s.in_progress()) {
        return refuse("the hub is updating. Try again when it finishes.".into());
    }
    let name = &req.machine;
    let Some((machine, link)) = app.machines.find(name) else {
        return Err((StatusCode::NOT_FOUND, format!("no machine named {name}")));
    };
    if machine.guest {
        return Err((
            StatusCode::FORBIDDEN,
            format!("{name} is a guest machine. Only your own machines can become the hub."),
        ));
    }
    let Some((link, address)) = link else {
        return refuse(format!("{name} is offline"));
    };
    if machine.update.as_ref().is_some_and(|s| s.in_progress()) {
        return refuse(format!("{name} is updating. Try again when it finishes."));
    }
    if machine.version != update::CURRENT {
        return refuse(format!(
            "{name} runs v{} and the hub runs v{}. Update both to the same version first.",
            machine.version,
            update::CURRENT
        ));
    }
    let Some(hub) = machine.tailnet_name else {
        return refuse(format!("the hub does not know {name}'s tailnet name"));
    };
    if machine.has_battery && !req.force {
        return refuse(format!(
            "{name} has a battery, so it may leave home and change IP. Pass force to make it the hub anyway."
        ));
    }
    Ok(Target { link, hub, address })
}

pub async fn start(State(app): State<Arc<App>>, Json(req): Json<MoveRequest>) -> Response {
    let target = match check_move(&app, &req) {
        Ok(target) => target,
        Err((status, message)) => return fail(status, message),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let moving = HubState::Moving {
        id: id.clone(),
        hub: target.hub.clone(),
        address: Some(target.address),
    };
    match app.hub_state.compare_and_set(&HubState::Active, moving) {
        Ok(true) => {}
        Ok(false) => return fail(StatusCode::CONFLICT, "the hub is not active"),
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
    tokio::spawn(resume_after(app.clone(), id.clone(), MOVE_TIMEOUT));
    app.refresher.close_gate().await;
    app.usage.set_paused(true);
    let _ = app.ws_shutdown.send(true);
    let sent = export_accounts(app.store.dir()).map(|files| {
        let command = json!({
            "type": "become_hub",
            "id": id,
            "files": files,
            "management_key": app.management_key,
            "guests": app.guests.export(),
        });
        app.machines.send_to_link(target.link, command)
    });
    let failure = match sent {
        Ok(true) => None,
        Ok(false) => Some(fail(
            StatusCode::CONFLICT,
            format!("{} disconnected before the move started", req.machine),
        )),
        Err(e) => Some(fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
    };
    if let Some(failure) = failure {
        let _ = restore_service(&app, &id).await;
        return failure;
    }
    Json(json!({ "status": "moving", "hub": target.hub })).into_response()
}

#[derive(Deserialize)]
pub struct Commit {
    id: String,
}

pub fn commit_response(ok: bool) -> Response {
    Json(json!({ "status": if ok { "ok" } else { "aborted" } })).into_response()
}

fn not_the_target() -> Response {
    fail(StatusCode::FORBIDDEN, "only the machine taking over can do this")
}

pub async fn commit(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    Json(req): Json<Commit>,
) -> Response {
    let state = app.hub_state.get();
    let (HubState::Moving { id, hub, address } | HubState::Retired { id, hub, address }) = state.clone() else {
        return commit_response(false);
    };
    if id != req.id {
        return commit_response(false);
    }
    if address != Some(from.ip()) {
        return not_the_target();
    }
    if matches!(state, HubState::Moving { .. }) {
        match app
            .hub_state
            .compare_and_set(&state, HubState::Retired { id, hub, address })
        {
            Ok(true) => {
                tokio::spawn(follow_new_hub(app.clone()));
            }
            Ok(false) if matches!(app.hub_state.get(), HubState::Retired { id, .. } if id == req.id) => {}
            Ok(false) => return commit_response(false),
            Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        }
    }
    if let Err(e) = finish_retirement(&app.paths, &req.id) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    if let Some(hub) = app.redirect_hub() {
        app.machines.broadcast(&crate::machines::hub_moved(&hub));
    }
    commit_response(true)
}

#[derive(Deserialize)]
pub struct Abort {
    id: String,
}

pub async fn abort(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    Json(req): Json<Abort>,
) -> Response {
    if let HubState::Moving { id, address, .. } = app.hub_state.get()
        && id == req.id
        && address != Some(from.ip())
    {
        return not_the_target();
    }
    match restore_service(&app, &req.id).await {
        Ok(true) => Json(json!({ "status": "aborted" })).into_response(),
        Ok(false) => fail(StatusCode::CONFLICT, "the hub is not moving under that transfer"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub async fn resume(State(app): State<Arc<App>>) -> Response {
    let HubState::Moving { id, .. } = app.hub_state.get() else {
        return fail(StatusCode::CONFLICT, "the hub is not in the middle of a move");
    };
    match restore_service(&app, &id).await {
        Ok(true) => Json(json!({ "status": "active" })).into_response(),
        Ok(false) => fail(StatusCode::CONFLICT, "the move finished before this could take effect"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub fn continue_move(app: &Arc<App>) {
    match app.hub_state.get() {
        HubState::Moving { id, .. } => {
            tokio::spawn(resume_after(app.clone(), id, MOVE_TIMEOUT));
        }
        HubState::Retired { .. } => {
            tokio::spawn(follow_new_hub(app.clone()));
        }
        HubState::Active | HubState::Aborted { .. } => {}
    }
}

async fn serves_as_hub(app: &App, url: &str) -> bool {
    let reply = app.control_http.get(url).timeout(Duration::from_secs(10)).send().await;
    match reply {
        Ok(r) => r.json::<Value>().await.is_ok_and(|v| v["role"] == "hub"),
        Err(_) => false,
    }
}

pub async fn follow_new_hub(app: Arc<App>) {
    let Some(hub) = app.redirect_hub() else { return };
    let url = format!("http://{hub}:{}/conflux/api/status", app.cfg.port);
    while !serves_as_hub(&app, &url).await {
        tokio::time::sleep(RETRY_EVERY).await;
    }
    if let Err(e) = become_node(&app, &hub).await {
        tracing::error!("could not turn this machine into a node of {hub}: {e:#}");
    }
}

async fn become_node(app: &Arc<App>, hub: &str) -> Result<()> {
    if app.lifecycle.is_draining() {
        return Ok(());
    }
    let mut cfg = Config::load(&app.paths)?;
    cfg.role = Role::Node;
    cfg.hub = Some(hub.to_string());
    cfg.save(&app.paths)?;
    fsutil::remove_durable(&hub_state_file(&app.paths))?;
    tracing::info!(hub, "the new hub is serving; this machine is now its node");
    hand_off_and_drain(&app.lifecycle, app.updater.exe(), &**app).await;
    app.lifecycle.close_listeners();
    Ok(())
}

pub async fn hand_off_and_drain<H: update::Handoff>(
    lifecycle: &crate::lifecycle::Lifecycle,
    exe: Option<&Path>,
    host: &H,
) {
    let handed_over = match exe {
        Some(exe) => update::hand_off(lifecycle, exe, host).await,
        None => Err(anyhow::anyhow!("this conflux does not know which binary to start")),
    };
    if let Err(e) = handed_over {
        tracing::error!("could not start the next conflux; this one stops when its requests finish: {e:#}");
    }
    lifecycle.begin_drain();
}

fn is_always_allowed(path: &str) -> bool {
    path == "/conflux/control" || path.starts_with("/conflux/move/")
}

pub async fn state_guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let state = app.hub_state.get();
    let message = match &state {
        HubState::Active | HubState::Aborted { .. } => return next.run(req).await,
        HubState::Moving { .. } => {
            let read_only = req.method() == http::Method::GET
                && (path == "/" || path.starts_with("/conflux/api/dashboard") || path == "/conflux/api/status");
            let resume = path == "/conflux/api/hub/resume";
            if is_always_allowed(&path) || read_only || resume {
                return next.run(req).await;
            }
            "conflux: the hub is moving to another machine. Try again in a minute.".to_string()
        }
        HubState::Retired { hub, .. } => {
            if is_always_allowed(&path) {
                return next.run(req).await;
            }
            if path == "/" && req.method() == http::Method::GET {
                return Redirect::temporary(&format!("http://{hub}:{}/", app.cfg.port)).into_response();
            }
            format!("conflux: the hub moved to {hub}. This machine follows it within a minute. Try again then.")
        }
    };
    let ua = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    match proxy::provider_for(&path, &ua) {
        Some(p) => proxy::provider_error(p, StatusCode::SERVICE_UNAVAILABLE, "api_error", &message),
        None => fail(StatusCode::SERVICE_UNAVAILABLE, message),
    }
}

pub fn is_laptop() -> bool {
    let output_contains = |cmd: &str, args: &[&str], needle: &str| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(needle))
    };
    if cfg!(target_os = "macos") {
        return output_contains("pmset", &["-g", "batt"], "InternalBattery");
    }
    if cfg!(windows) {
        let query = "Get-CimInstance Win32_Battery | Select-Object -ExpandProperty Name";
        return std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", query])
            .output()
            .is_ok_and(|o| !o.stdout.iter().all(u8::is_ascii_whitespace));
    }
    std::fs::read_dir("/sys/class/power_supply").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("BAT"))
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub old_hub: String,
    pub port: u16,
    pub management_key: String,
    #[serde(default)]
    pub guests: GuestList,
}

fn incoming_root(paths: &Paths) -> PathBuf {
    paths.data_dir.join("incoming")
}

fn transfer_file(dir: &Path) -> PathBuf {
    dir.join("transfer.json")
}

pub fn find_pending(paths: &Paths) -> Result<Option<(PathBuf, Pending)>> {
    let root = incoming_root(paths);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if let Some(p) = fsutil::read_json::<Pending>(&transfer_file(&dir))? {
            return Ok(Some((dir, p)));
        }
    }
    Ok(None)
}

pub fn import_bundle(paths: &Paths, bundle: &Bundle, old_hub: &str, port: u16) -> Result<(PathBuf, Pending)> {
    uuid::Uuid::parse_str(&bundle.id).context("the move has an invalid id")?;
    let dir = incoming_root(paths).join(&bundle.id);
    let imported = write_bundle(&dir, bundle, old_hub, port);
    if imported.is_err() && dir.exists() {
        let _ = discard_incoming(&dir);
    }
    imported.map(|pending| (dir, pending))
}

fn write_bundle(dir: &Path, bundle: &Bundle, old_hub: &str, port: u16) -> Result<Pending> {
    let files = dir.join("files");
    if let Some(root) = dir.parent() {
        fsutil::ensure_private_dir(root)?;
    }
    fsutil::ensure_private_dir(dir)?;
    fsutil::ensure_private_dir(&files)?;
    for f in &bundle.files {
        let rel = Path::new(&f.path);
        if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("the export contains an unsafe path: {}", f.path);
        }
        let target = files.join(rel);
        if let Some(parent) = target.parent() {
            fsutil::ensure_private_dir(parent)?;
        }
        fsutil::write_durable(&target, &STANDARD.decode(&f.content)?)?;
    }
    let pending = Pending {
        id: bundle.id.clone(),
        old_hub: old_hub.to_string(),
        port,
        management_key: bundle.management_key.clone(),
        guests: bundle.guests.clone(),
    };
    fsutil::write_json_durable(&transfer_file(dir), &pending)?;
    Ok(pending)
}

pub fn activate(paths: &Paths, dir: &Path, pending: &Pending) -> Result<()> {
    let files = dir.join("files");
    let target = paths.accounts_dir();
    if files.exists() {
        if target.exists() {
            let empty = std::fs::read_dir(&target)?.next().is_none();
            if !empty {
                bail!("{} already has accounts in it", target.display());
            }
            std::fs::remove_dir(&target)?;
        }
        fsutil::ensure_private_dir(&paths.data_dir)?;
        std::fs::rename(&files, &target).context("move the imported accounts into place")?;
        fsutil::sync_dir(&paths.data_dir)?;
    }
    fsutil::ensure_private_dir(&paths.config_dir)?;
    fsutil::write_durable(&paths.management_key_file(), pending.management_key.as_bytes())?;
    fsutil::write_json_durable(&guests::guests_file(paths), &pending.guests)?;
    Ok(())
}

pub fn discard_incoming(dir: &Path) -> Result<()> {
    std::fs::remove_dir_all(dir)?;
    if let Some(parent) = dir.parent() {
        fsutil::sync_dir(parent)?;
    }
    Ok(())
}

pub async fn commit_move(http: &reqwest::Client, pending: &Pending) -> Result<bool> {
    let url = format!("http://{}:{}/conflux/move/commit", pending.old_hub, pending.port);
    for attempt in 0..COMMIT_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(RETRY_EVERY).await;
        }
        let sent = http
            .post(&url)
            .json(&json!({ "id": pending.id }))
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        let Ok(resp) = sent else { continue };
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        match v["status"].as_str() {
            Some("ok") => return Ok(true),
            Some("aborted") => return Ok(false),
            _ => continue,
        }
    }
    bail!("could not confirm the move with {}", pending.old_hub)
}

pub fn become_hub(paths: &Paths, dir: &Path, pending: &Pending) -> Result<()> {
    activate(paths, dir, pending)?;
    fsutil::remove_durable(&hub_state_file(paths))?;
    let mut cfg = Config::load(paths)?;
    cfg.role = Role::Hub;
    cfg.hub = None;
    cfg.save(paths)?;
    discard_incoming(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> Bundle {
        Bundle {
            id: "11111111-1111-4111-8111-111111111111".into(),
            files: vec![
                BundleFile {
                    path: "a.json".into(),
                    content: STANDARD.encode(b"{}"),
                },
                BundleFile {
                    path: "a.journal/claim-2".into(),
                    content: STANDARD.encode(b"x"),
                },
            ],
            management_key: "key".into(),
            guests: GuestList::default(),
        }
    }

    #[test]
    fn import_then_activate() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(d.path());
        let (dir, pending) = import_bundle(&paths, &bundle(), "hub-box", 8317).unwrap();
        assert!(!paths.accounts_dir().exists(), "import stays inert");
        let (found, _) = find_pending(&paths).unwrap().unwrap();
        assert_eq!(found, dir);
        activate(&paths, &dir, &pending).unwrap();
        assert!(paths.accounts_dir().join("a.journal/claim-2").exists());
        activate(&paths, &dir, &pending).unwrap();
        discard_incoming(&dir).unwrap();
        assert!(find_pending(&paths).unwrap().is_none());
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut b = bundle();
        b.files[1].path = "../../evil".into();
        let paths = Paths::under(d.path());
        assert!(import_bundle(&paths, &b, "hub-box", 8317).is_err());
        assert!(
            !incoming_root(&paths).join(&b.id).exists(),
            "a failed import leaves no tokens behind"
        );
        let mut b = bundle();
        b.id = "../../evil".into();
        assert!(import_bundle(&paths, &b, "hub-box", 8317).is_err());
    }

    #[test]
    fn activation_refuses_to_overwrite_accounts() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(d.path());
        let (dir, pending) = import_bundle(&paths, &bundle(), "hub-box", 8317).unwrap();
        fsutil::ensure_private_dir(&paths.accounts_dir()).unwrap();
        std::fs::write(paths.accounts_dir().join("x.json"), "{}").unwrap();
        assert!(activate(&paths, &dir, &pending).is_err());
    }

    #[test]
    fn retirement_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(d.path());
        fsutil::ensure_private_dir(&paths.accounts_dir()).unwrap();
        finish_retirement(&paths, "id1").unwrap();
        assert!(!paths.accounts_dir().exists());
        assert!(retired_dir(&paths, "id1").exists());
        finish_retirement(&paths, "id1").unwrap();
        fsutil::ensure_private_dir(&paths.accounts_dir()).unwrap();
        finish_retirement(&paths, "id1").unwrap();
        assert!(retired_dir(&paths, "id1").exists());
    }
}

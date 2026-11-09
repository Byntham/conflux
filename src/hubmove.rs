use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::App;
use crate::config::Paths;
use crate::fsutil;
use crate::hubstate::HubState;
use crate::proxy;

const CODE_LIFETIME: Duration = Duration::from_secs(600);
const MAX_CODE_ATTEMPTS: u32 = 5;
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub struct MoveCode {
    code: String,
    created: Instant,
    failures: u32,
}

impl MoveCode {
    fn generate() -> Self {
        let mut rng = rand::rng();
        let code = (0..8)
            .map(|_| CODE_ALPHABET[rng.random_range(0..CODE_ALPHABET.len())] as char)
            .collect();
        Self {
            code,
            created: Instant::now(),
            failures: 0,
        }
    }
}

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

fn local_only() -> Response {
    fail(StatusCode::FORBIDDEN, "run this command on the hub itself")
}

pub async fn create_code(State(app): State<Arc<App>>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    if !addr.ip().is_loopback() {
        return local_only();
    }
    if app.hub_state.get() != HubState::Active {
        return fail(StatusCode::CONFLICT, "the hub is not active");
    }
    let code = MoveCode::generate();
    let text = code.code.clone();
    *app.move_code.lock().unwrap() = Some(code);
    Json(json!({ "code": text, "expires_in_seconds": CODE_LIFETIME.as_secs() })).into_response()
}

fn check_code(app: &App, presented: &str) -> bool {
    let mut slot = app.move_code.lock().unwrap();
    let Some(code) = slot.as_mut() else { return false };
    if code.created.elapsed() > CODE_LIFETIME {
        *slot = None;
        return false;
    }
    if code.code == presented.trim().to_ascii_uppercase() {
        *slot = None;
        return true;
    }
    code.failures += 1;
    if code.failures >= MAX_CODE_ATTEMPTS {
        *slot = None;
    }
    false
}

async fn restore_service(app: &App, id: &str) -> Result<bool> {
    let moving = HubState::Moving { id: id.to_string() };
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

#[derive(Deserialize)]
pub struct Prepare {
    code: String,
    id: String,
}

pub async fn prepare(State(app): State<Arc<App>>, Json(req): Json<Prepare>) -> Response {
    if uuid::Uuid::parse_str(&req.id).is_err() {
        return fail(StatusCode::BAD_REQUEST, "invalid transfer id");
    }
    if !check_code(&app, &req.code) {
        return fail(
            StatusCode::FORBIDDEN,
            "wrong or expired move code. Run `conflux hub move-code` on the hub.",
        );
    }
    let moving = HubState::Moving { id: req.id.clone() };
    match app.hub_state.compare_and_set(&HubState::Active, moving) {
        Ok(true) => {}
        Ok(false) => return fail(StatusCode::CONFLICT, "the hub is not active"),
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
    app.refresher.close_gate().await;
    app.usage.set_paused(true);
    let _ = app.ws_shutdown.send(true);
    match export_accounts(app.store.dir()) {
        Ok(files) => Json(Bundle {
            id: req.id,
            files,
            management_key: app.management_key.clone(),
        })
        .into_response(),
        Err(e) => {
            let _ = restore_service(&app, &req.id).await;
            fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
        }
    }
}

#[derive(Deserialize)]
pub struct Commit {
    id: String,
    hub: String,
}

pub fn commit_response(ok: bool) -> Response {
    Json(json!({ "status": if ok { "ok" } else { "aborted" } })).into_response()
}

pub async fn commit(State(app): State<Arc<App>>, Json(req): Json<Commit>) -> Response {
    let moving = HubState::Moving { id: req.id.clone() };
    let retired = HubState::Retired {
        id: req.id.clone(),
        hub: req.hub.clone(),
    };
    let result = match app.hub_state.get() {
        state if state == moving => match app.hub_state.compare_and_set(&moving, retired) {
            Ok(true) => true,
            Ok(false) => matches!(app.hub_state.get(), HubState::Retired { id, .. } if id == req.id),
            Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        },
        HubState::Retired { id, .. } if id == req.id => true,
        _ => false,
    };
    if !result {
        return commit_response(false);
    }
    if let Err(e) = finish_retirement(&app.paths, &req.id) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    }
    commit_response(true)
}

#[derive(Deserialize)]
pub struct Abort {
    id: String,
}

pub async fn abort(State(app): State<Arc<App>>, Json(req): Json<Abort>) -> Response {
    match restore_service(&app, &req.id).await {
        Ok(true) => Json(json!({ "status": "aborted" })).into_response(),
        Ok(false) => fail(StatusCode::CONFLICT, "the hub is not moving under that transfer"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

pub async fn resume(State(app): State<Arc<App>>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    if !addr.ip().is_loopback() {
        return local_only();
    }
    let HubState::Moving { id } = app.hub_state.get() else {
        return fail(StatusCode::CONFLICT, "the hub is not in the middle of a move");
    };
    match restore_service(&app, &id).await {
        Ok(true) => Json(json!({ "status": "active" })).into_response(),
        Ok(false) => fail(StatusCode::CONFLICT, "the move finished before this could take effect"),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn is_always_allowed(path: &str) -> bool {
    path == "/conflux/heartbeat" || path.starts_with("/conflux/move/")
}

pub async fn state_guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let state = app.hub_state.get();
    let message = match &state {
        HubState::Active | HubState::Aborted { .. } => return next.run(req).await,
        HubState::Moving { .. } => {
            let read_only = req.method() == http::Method::GET
                && (path == "/" || path == "/conflux/api/dashboard" || path == "/conflux/api/status");
            let local_resume = path == "/conflux/api/hub/resume";
            if is_always_allowed(&path) || read_only || local_resume {
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
            format!("conflux: the hub moved to {hub}. On this machine, run `conflux join {hub}`.")
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
    let dir = incoming_root(paths).join(&bundle.id);
    let files = dir.join("files");
    fsutil::ensure_private_dir(&incoming_root(paths))?;
    fsutil::ensure_private_dir(&dir)?;
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
    };
    fsutil::write_json_durable(&transfer_file(&dir), &pending)?;
    Ok((dir, pending))
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
    Ok(())
}

pub fn discard_incoming(dir: &Path) -> Result<()> {
    std::fs::remove_dir_all(dir)?;
    if let Some(parent) = dir.parent() {
        fsutil::sync_dir(parent)?;
    }
    Ok(())
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
        b.files[0].path = "../../evil".into();
        assert!(import_bundle(&Paths::under(d.path()), &b, "hub-box", 8317).is_err());
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

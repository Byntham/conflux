use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::App;

pub const ONLINE_WINDOW: chrono::Duration = chrono::Duration::minutes(2);
pub const REPO_URL: &str = "https://github.com/Byntham/conflux";
pub const REPO_SLUG: &str = "Byntham/conflux";

#[derive(Clone, Debug, Serialize)]
pub struct Machine {
    pub hostname: String,
    pub version: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub online: bool,
    pub transcript_sync: bool,
}

#[derive(Default)]
pub struct Machines {
    map: Mutex<BTreeMap<String, Machine>>,
}

impl Machines {
    pub fn seen(&self, hostname: &str, version: &str, role: &str, transcript_sync: bool) {
        self.map.lock().unwrap().insert(
            hostname.to_string(),
            Machine {
                hostname: hostname.into(),
                version: version.into(),
                role: role.into(),
                last_seen: Utc::now(),
                online: true,
                transcript_sync,
            },
        );
    }

    pub fn list(&self) -> Vec<Machine> {
        let now = Utc::now();
        self.map
            .lock()
            .unwrap()
            .values()
            .cloned()
            .map(|mut m| {
                m.online = now - m.last_seen < ONLINE_WINDOW;
                m
            })
            .collect()
    }
}

#[derive(Deserialize)]
pub struct Heartbeat {
    hostname: String,
    version: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    transcript_sync: bool,
}

pub async fn heartbeat(State(app): State<Arc<App>>, Json(hb): Json<Heartbeat>) -> Json<Value> {
    app.machines.seen(
        &hb.hostname,
        &hb.version,
        hb.role.as_deref().unwrap_or("node"),
        hb.transcript_sync,
    );
    Json(json!({ "hub": app.redirect_hub() }))
}

pub fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn stored_build(builds_dir: &FsPath, platform: &str) -> Option<PathBuf> {
    [format!("conflux-{platform}"), format!("conflux-{platform}.exe")]
        .into_iter()
        .map(|name| builds_dir.join(name))
        .find(|p| p.is_file())
}

fn is_known_platform(p: &str) -> bool {
    !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub async fn download(State(app): State<Arc<App>>, Path(requested): Path<String>) -> Response {
    if !is_known_platform(&requested) {
        return (StatusCode::BAD_REQUEST, "invalid platform").into_response();
    }
    let source = if requested == platform() {
        std::env::current_exe().ok()
    } else {
        stored_build(&app.paths.builds_dir(), &requested)
    };
    let Some(source) = source else {
        let hint = format!(
            "this hub has no build for {requested}. On the hub, run `conflux hub fetch-builds`, then try again."
        );
        return (StatusCode::NOT_FOUND, hint).into_response();
    };
    match std::fs::read(source) {
        Ok(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes))
            .expect("valid response"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub fn fetch_builds(builds_dir: &FsPath) -> anyhow::Result<usize> {
    crate::fsutil::ensure_private_dir(builds_dir)?;
    let status = std::process::Command::new("gh")
        .args([
            "release",
            "download",
            "--repo",
            REPO_SLUG,
            "--pattern",
            "conflux-*",
            "--clobber",
            "--dir",
        ])
        .arg(builds_dir)
        .status()
        .context("run gh; install the GitHub CLI and sign in with `gh auth login`")?;
    if !status.success() {
        anyhow::bail!("gh could not download the latest release of {REPO_SLUG}");
    }
    let count = std::fs::read_dir(builds_dir)?
        .flatten()
        .filter(|e| e.path().is_file())
        .count();
    for entry in std::fs::read_dir(builds_dir)?.flatten() {
        make_executable(&entry.path())?;
    }
    Ok(count)
}

#[cfg(unix)]
fn make_executable(path: &FsPath) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &FsPath) -> anyhow::Result<()> {
    Ok(())
}

pub fn install_powershell(hub: &str, port: u16) -> String {
    format!(
        r#"$ErrorActionPreference = "Stop"
$Hub = "{hub}"
$Port = "{port}"
$Dir = Join-Path $env:LOCALAPPDATA "conflux"
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
$Exe = Join-Path $Dir "conflux.exe"
Get-Process conflux -ErrorAction SilentlyContinue | Stop-Process -Force
try {{
  Invoke-WebRequest -UseBasicParsing -Uri "http://${{Hub}}:${{Port}}/conflux/download/windows-x86_64" -OutFile $Exe
}} catch {{
  Invoke-WebRequest -UseBasicParsing -Uri "{REPO_URL}/releases/latest/download/conflux-windows-x86_64.exe" -OutFile $Exe
}}
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not (($UserPath -split ";") -contains $Dir)) {{
  [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dir", "User")
}}
& $Exe join $Hub
"#
    )
}

pub fn install_script(hub: &str, port: u16) -> String {
    format!(
        r#"#!/bin/sh
set -eu
HUB="{hub}"
PORT="{port}"
BIN_DIR="${{CONFLUX_BIN_DIR:-$HOME/.local/bin}}"
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$arch" in
  amd64) arch=x86_64 ;;
  arm64) arch=aarch64 ;;
esac
mkdir -p "$BIN_DIR"
fetch() {{ curl -fsSL -o "$BIN_DIR/conflux.download" "$1"; }}
if fetch "http://$HUB:$PORT/conflux/download/$os-$arch" || fetch "{REPO_URL}/releases/latest/download/conflux-$os-$arch"; then
  chmod +x "$BIN_DIR/conflux.download"
  mv "$BIN_DIR/conflux.download" "$BIN_DIR/conflux"
elif command -v cargo >/dev/null 2>&1; then
  rm -f "$BIN_DIR/conflux.download"
  echo "No $os-$arch build on the hub. Building from source with cargo..."
  cargo install --locked --git {REPO_URL} --root "$(dirname "$BIN_DIR")" conflux
else
  rm -f "$BIN_DIR/conflux.download"
  echo "No $os-$arch build on the hub, and cargo is not installed." >&2
  echo "Install Rust from https://rustup.rs, then run this again." >&2
  exit 1
fi
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: add $BIN_DIR to your PATH." ;;
esac
"$BIN_DIR/conflux" join "$HUB"
"#
    )
}

fn requested_hub_name(app: &App, headers: &HeaderMap) -> String {
    crate::dashboard::hub_name(app, headers.get(header::HOST).and_then(|v| v.to_str().ok()))
}

fn text_response(content_type: &'static str, body: String) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("valid response")
}

pub async fn install_sh(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    text_response(
        "text/x-shellscript",
        install_script(&requested_hub_name(&app, &headers), app.cfg.port),
    )
}

pub async fn install_ps1(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    text_response(
        "text/plain",
        install_powershell(&requested_hub_name(&app, &headers), app.cfg.port),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_targets_the_hub() {
        let s = install_script("hub-box", 8317);
        assert!(s.contains("HUB=\"hub-box\""));
        assert!(s.contains("http://$HUB:$PORT/conflux/download/$os-$arch"));
        assert!(s.contains("/releases/latest/download/conflux-$os-$arch"));
        assert!(s.contains("\"$BIN_DIR/conflux\" join \"$HUB\""));
    }

    #[test]
    fn powershell_targets_the_hub() {
        let s = install_powershell("hub-box", 8317);
        assert!(s.contains("$Hub = \"hub-box\""));
        assert!(s.contains("http://${Hub}:${Port}/conflux/download/windows-x86_64"));
        assert!(s.contains("& $Exe join $Hub"));
    }

    #[test]
    fn stored_builds_are_found_by_platform() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("conflux-windows-x86_64.exe"), "x").unwrap();
        std::fs::write(d.path().join("conflux-linux-aarch64"), "x").unwrap();
        assert!(stored_build(d.path(), "windows-x86_64").is_some());
        assert!(stored_build(d.path(), "linux-aarch64").is_some());
        assert!(stored_build(d.path(), "darwin-aarch64").is_none());
        assert!(!is_known_platform("../../etc"));
        assert!(is_known_platform("windows-x86_64"));
    }

    #[test]
    fn online_window() {
        let m = Machines::default();
        m.seen("node-one", "0.1.0", "node", false);
        assert!(m.list()[0].online);
    }
}

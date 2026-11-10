use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

use crate::lifecycle::Lifecycle;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

pub fn parse_version(v: &str) -> Option<[u64; 3]> {
    let mut parts = v.split('.').map(|p| {
        p.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| p.parse::<u64>().ok())
            .flatten()
    });
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(version)
}

pub fn is_newer(candidate: &str, than: &str) -> bool {
    match (parse_version(candidate), parse_version(than)) {
        (Some(candidate), Some(than)) => candidate > than,
        _ => false,
    }
}

pub fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("conflux-linux-x86_64"),
        ("linux", "aarch64") => Some("conflux-linux-aarch64"),
        ("macos", "aarch64") => Some("conflux-darwin-aarch64"),
        ("windows", "x86_64") => Some("conflux-windows-x86_64.exe"),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum UpdateStatus {
    Downloading { version: String },
    Starting { version: String },
    Failed { version: String, error: String },
}

impl UpdateStatus {
    pub fn in_progress(&self) -> bool {
        !matches!(self, UpdateStatus::Failed { .. })
    }
}

pub trait Handoff: Send + Sync + 'static {
    fn pause(&self) -> impl Future<Output = (Value, mpsc::UnboundedReceiver<Value>)> + Send;
    fn resume(&self) -> impl Future<Output = ()> + Send;
}

pub struct Updater {
    releases: String,
    http: reqwest::Client,
    exe: OnceLock<PathBuf>,
    status: watch::Sender<Option<UpdateStatus>>,
    latest: RwLock<Option<String>>,
}

fn sibling(exe: &Path, tag: &str) -> PathBuf {
    let stem = exe.file_stem().unwrap_or_default().to_string_lossy();
    let name = match exe.extension() {
        Some(ext) => format!("{stem}.{tag}.{}", ext.to_string_lossy()),
        None => format!("{stem}.{tag}"),
    };
    exe.with_file_name(name)
}

pub fn remove_old_binaries(exe: &Path) {
    let (Some(dir), Some(stem)) = (exe.parent(), exe.file_stem()) else {
        return;
    };
    let prefix = format!("{}.old-", stem.to_string_lossy());
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

async fn verify(binary: &Path) -> Result<()> {
    let mut attempts = 0;
    let output = loop {
        let run = tokio::process::Command::new(binary)
            .arg("--version")
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(VERIFY_TIMEOUT, run).await {
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 20 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(output) => break output.context("run the downloaded binary")?,
            Err(_) => bail!("the downloaded binary did not answer `--version`"),
        }
    };
    if !output.status.success() || !output.stdout.starts_with(b"conflux ") {
        bail!("the downloaded file is not a working conflux binary");
    }
    Ok(())
}

impl Updater {
    pub fn new(releases: &str) -> Arc<Self> {
        Arc::new(Self {
            releases: releases.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .user_agent(format!("conflux/{CURRENT}"))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("build release client"),
            exe: OnceLock::new(),
            status: watch::channel(None).0,
            latest: RwLock::new(None),
        })
    }

    pub fn replaces(&self, exe: PathBuf) {
        let _ = self.exe.set(exe);
    }

    pub fn status(&self) -> Option<UpdateStatus> {
        self.status.borrow().clone()
    }

    pub fn watch_status(&self) -> watch::Receiver<Option<UpdateStatus>> {
        self.status.subscribe()
    }

    pub fn latest(&self) -> Option<String> {
        self.latest.read().unwrap().clone()
    }

    pub fn set_latest(&self, version: Option<String>) {
        *self.latest.write().unwrap() = version;
    }

    pub async fn check_latest(&self) -> Result<String> {
        let resp = self
            .http
            .get(format!("{}/latest", self.releases))
            .timeout(CHECK_TIMEOUT)
            .send()
            .await
            .context("reach the release page")?;
        if !resp.status().is_success() {
            bail!("the release page answered HTTP {}", resp.status());
        }
        let tag = resp.url().path().trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let version = tag
            .strip_prefix('v')
            .filter(|v| parse_version(v).is_some())
            .with_context(|| format!("the latest release `{tag}` is not a version"))?
            .to_string();
        *self.latest.write().unwrap() = Some(version.clone());
        Ok(version)
    }

    pub async fn run_checks(self: Arc<Self>) {
        loop {
            if let Err(e) = self.check_latest().await {
                tracing::debug!("could not check for a new release: {e:#}");
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    }

    async fn download(&self, version: &str, to: &Path) -> Result<()> {
        let asset =
            asset_name().context("conflux has no prebuilt binary for this system. Update it with `cargo install`.")?;
        let url = format!("{}/download/v{version}/{asset}", self.releases);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("download {url}"))?;
        let mut file = tokio::fs::File::create(to)
            .await
            .with_context(|| format!("create {}", to.display()))?;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            file.write_all(&chunk.context("download interrupted")?).await?;
        }
        file.flush().await?;
        drop(file.into_std().await);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    async fn install(&self, version: &str, exe: &Path) -> Result<PathBuf> {
        let downloaded = sibling(exe, "download");
        let fetched = async {
            self.download(version, &downloaded).await?;
            verify(&downloaded).await
        }
        .await;
        if let Err(e) = fetched {
            let _ = std::fs::remove_file(&downloaded);
            return Err(e);
        }
        let backup = sibling(exe, &format!("old-{CURRENT}"));
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(exe, &backup).with_context(|| format!("move {} aside", exe.display()))?;
        if let Err(e) = std::fs::rename(&downloaded, exe) {
            let _ = std::fs::rename(&backup, exe);
            let _ = std::fs::remove_file(&downloaded);
            return Err(e).with_context(|| format!("replace {}", exe.display()));
        }
        Ok(backup)
    }

    async fn replace_and_hand_over<H: Handoff>(&self, version: &str, lifecycle: &Lifecycle, host: &H) -> Result<()> {
        if !is_newer(version, CURRENT) {
            bail!("v{version} is not newer than the running v{CURRENT}");
        }
        if lifecycle.is_draining() {
            bail!("a newer conflux has already taken over");
        }
        let exe = self
            .exe
            .get()
            .context("this conflux does not know which binary to replace")?;
        let backup = self.install(version, exe).await?;
        self.status.send_replace(Some(UpdateStatus::Starting {
            version: version.to_string(),
        }));
        let handed_over = async {
            let successor = lifecycle.spawn_successor(exe).await?;
            let (state, later) = host.pause().await;
            let result = successor.hand_over(&state, later).await;
            if result.is_err() {
                host.resume().await;
            }
            result
        }
        .await;
        if let Err(e) = handed_over {
            if let Err(restore) = std::fs::rename(&backup, exe) {
                tracing::error!("could not put the previous binary back: {restore}");
            }
            return Err(e);
        }
        lifecycle.begin_drain();
        Ok(())
    }

    pub async fn run<H: Handoff>(self: Arc<Self>, version: String, lifecycle: Arc<Lifecycle>, host: Arc<H>) {
        let mut started = false;
        self.status.send_if_modified(|status| {
            if status.as_ref().is_some_and(UpdateStatus::in_progress) {
                return false;
            }
            *status = Some(UpdateStatus::Downloading {
                version: version.clone(),
            });
            started = true;
            true
        });
        if !started {
            return;
        }
        tracing::info!(version, "updating conflux");
        if let Err(e) = self.replace_and_hand_over(&version, &lifecycle, &*host).await {
            tracing::error!(version, "update failed: {e:#}");
            self.status.send_replace(Some(UpdateStatus::Failed {
                version,
                error: format!("{e:#}"),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_newer_versions_count() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        for odd in ["1.2", "1.2.3.4", "1.2.x", "+1.2.3", "1.2.3/../x", "build-7", ""] {
            assert!(!is_newer(odd, "0.0.1"), "{odd}");
        }
    }
}

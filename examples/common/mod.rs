#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use conflux::accounts::Store;
use conflux::config::{Config, Endpoints, Paths, Role};
use conflux::oauth::{self, LoginTokens};
use conflux::refresh::Refresher;
use conflux::setup::{self, ClientPaths};
use tokio::process::{Child, Command};

pub fn build_conflux() -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = std::process::Command::new(cargo);
    build
        .args(["build", "--bin", "conflux"])
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    if !build.status().context("run cargo")?.success() {
        bail!("could not build conflux");
    }
    let exe = std::env::current_exe()?;
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .context("find the target directory")?;
    Ok(profile_dir.join(format!("conflux{}", std::env::consts::EXE_SUFFIX)))
}

pub fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub fn endpoint_env(ep: &Endpoints) -> Vec<(&'static str, String)> {
    vec![
        ("CONFLUX_ANTHROPIC_API", ep.anthropic_api.clone()),
        ("CONFLUX_CLAUDE_AUTHORIZE", ep.claude_authorize.clone()),
        ("CONFLUX_CLAUDE_TOKEN", ep.claude_token.clone()),
        ("CONFLUX_CODEX_API", ep.codex_api.clone()),
        ("CONFLUX_CHATGPT_BACKEND", ep.chatgpt_backend.clone()),
        ("CONFLUX_OPENAI_AUTH", ep.openai_auth.clone()),
        ("CONFLUX_RELEASES", ep.releases.clone()),
    ]
}

pub struct Layout {
    pub dir: tempfile::TempDir,
    pub hub_home: PathBuf,
    pub node_home: PathBuf,
    pub hub_paths: Paths,
    pub hub_port: u16,
    pub node_port: u16,
}

impl Layout {
    pub fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let hub_home = dir.path().join("hub");
        let node_home = dir.path().join("node");
        let hub_paths = Paths::under(&hub_home);
        let node_paths = Paths::under(&node_home);
        let hub_port = free_port()?;
        let node_port = free_port()?;
        Config {
            role: Role::Hub,
            port: hub_port,
            ..Config::default()
        }
        .save(&hub_paths)?;
        Config {
            role: Role::Node,
            hub: Some("127.0.0.1".into()),
            port: node_port,
            ..Config::default()
        }
        .save(&node_paths)?;
        let clients = ClientPaths {
            claude_settings: dir.path().join("claude").join("settings.json"),
            codex_config: dir.path().join("codex").join("config.toml"),
        };
        setup::apply(&clients, &node_paths.config_dir, node_port)?;
        Ok(Self {
            dir,
            hub_home,
            node_home,
            hub_paths,
            hub_port,
            node_port,
        })
    }

    pub async fn start(
        &self,
        conflux: &Path,
        env: &[(&'static str, String)],
        log: Option<&Path>,
    ) -> Result<(Child, Child)> {
        let mut env = env.to_vec();
        let mut hub = self.machine(conflux, &self.hub_home, "dev-hub", log).start(&env)?;
        wait_until_up(&mut hub, self.hub_port, "the dev hub").await?;
        env.push(("CONFLUX_HUB_PORT", self.hub_port.to_string()));
        let mut node = self.machine(conflux, &self.node_home, "dev-node", log).start(&env)?;
        wait_until_up(&mut node, self.node_port, "the dev node").await?;
        Ok((hub, node))
    }

    fn machine<'a>(&'a self, conflux: &'a Path, home: &'a Path, name: &'a str, log: Option<&'a Path>) -> Machine<'a> {
        Machine {
            conflux,
            home,
            name,
            clients: self.dir.path(),
            log,
        }
    }
}

pub async fn add_accounts(paths: &Paths, ep: &Endpoints, logins: Vec<LoginTokens>) -> Result<()> {
    let store = Arc::new(Store::load(&paths.accounts_dir())?);
    let refresher = Refresher::new(store, ep.clone(), oauth::control_client(), true);
    for login in logins {
        refresher.install_login(login).await?;
    }
    Ok(())
}

struct Machine<'a> {
    conflux: &'a Path,
    home: &'a Path,
    name: &'a str,
    clients: &'a Path,
    log: Option<&'a Path>,
}

impl Machine<'_> {
    fn start(&self, env: &[(&'static str, String)]) -> Result<Child> {
        let mut command = Command::new(self.conflux);
        command
            .arg("worker")
            .env("CONFLUX_HOME", self.home)
            .env("CONFLUX_HOSTNAME", self.name)
            .env("CLAUDE_CONFIG_DIR", self.clients.join("claude"))
            .env("CODEX_HOME", self.clients.join("codex"))
            .env_remove("CONFLUX_HANDOFF")
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(dir) = self.log {
            let file = std::fs::File::create(dir.join(format!("{}.log", self.name)))?;
            command.stdout(file.try_clone()?).stderr(file).env("NO_COLOR", "1");
        }
        command.spawn().with_context(|| format!("start {}", self.name))
    }
}

async fn wait_until_up(child: &mut Child, port: u16, name: &str) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/conflux/api/status");
    let http = reqwest::Client::new();
    for _ in 0..300 {
        if let Some(status) = child.try_wait()? {
            bail!("{name} stopped before it was ready ({status})");
        }
        let up = http.get(&url).send().await.is_ok_and(|r| r.status().is_success());
        if up {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("{name} did not start in time")
}

pub async fn stop_requested() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("watch for SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

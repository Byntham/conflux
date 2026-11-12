use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::DefaultBodyLimit;
use axum::routing::{any, get, patch, post};
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::accounts::Store;
use crate::api::{self, Logins};
use crate::config::{Config, Endpoints, Paths};
use crate::hubmove::{self, MoveCode};
use crate::hubstate::{HubState, HubStateStore};
use crate::lifecycle::Lifecycle;
use crate::machines::{self, Machine, Machines};
use crate::oauth;
use crate::refresh::Refresher;
use crate::requests::RequestLog;
use crate::router::Router;
use crate::tailnet::{self, TailnetSelf, Tailscale};
use crate::transcript_sync::TranscriptSync;
use crate::transcripts::{self, Transcripts};
use crate::update::{self, Updater};
use crate::usage::Usage;
use crate::{access, dashboard, fsutil, link, management, proxy};

const TRANSCRIPT_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub struct App {
    pub cfg: Config,
    pub ep: Endpoints,
    pub store: Arc<Store>,
    pub refresher: Arc<Refresher>,
    pub usage: Arc<Usage>,
    pub router: Arc<Router>,
    pub proxy_http: reqwest::Client,
    pub control_http: reqwest::Client,
    pub logins: Logins,
    pub allowed_hosts: RwLock<Vec<String>>,
    pub machines: Machines,
    pub tailnet: RwLock<TailnetSelf>,
    pub hostname: String,
    pub paths: Paths,
    pub hub_state: HubStateStore,
    pub move_code: Mutex<Option<MoveCode>>,
    pub ws_shutdown: tokio::sync::watch::Sender<bool>,
    pub management_key: String,
    pub api_call_http: reqwest::Client,
    pub requests: Arc<RequestLog>,
    pub transcripts: Arc<Transcripts>,
    pub lifecycle: Arc<Lifecycle>,
    pub updater: Arc<Updater>,
    pub tailscale: Tailscale,
}

pub fn proxy_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build proxy client")
}

impl App {
    pub fn new(cfg: Config, paths: &Paths, ep: Endpoints) -> Result<Arc<Self>> {
        Self::on_tailnet(cfg, paths, ep, tailnet::system_tailscale())
    }

    pub fn on_tailnet(cfg: Config, paths: &Paths, ep: Endpoints, tailscale: Tailscale) -> Result<Arc<Self>> {
        Self::with_lifecycle(cfg, paths, ep, TailnetSelf::default(), Arc::default(), tailscale)
    }

    fn with_lifecycle(
        cfg: Config,
        paths: &Paths,
        ep: Endpoints,
        tailnet: TailnetSelf,
        lifecycle: Arc<Lifecycle>,
        tailscale: Tailscale,
    ) -> Result<Arc<Self>> {
        let hub_state = HubStateStore::load(&hubmove::hub_state_file(paths))?;
        match hub_state.get() {
            HubState::Aborted { id } => {
                hub_state.compare_and_set(&HubState::Aborted { id }, HubState::Active)?;
            }
            HubState::Retired { id, .. } => hubmove::finish_retirement(paths, &id)?,
            HubState::Active | HubState::Moving { .. } => {}
        }
        let moving = matches!(hub_state.get(), HubState::Moving { .. });
        let store = Arc::new(Store::load(&paths.accounts_dir())?);
        let control_http = oauth::control_client();
        let refresher = Arc::new(Refresher::new(store.clone(), ep.clone(), control_http.clone(), !moving));
        let usage = Arc::new(Usage::new(ep.clone(), control_http.clone()));
        usage.set_paused(moving);
        let router = Router::new(store.clone(), usage.clone());
        let mut allowed_hosts = access::local_hosts(cfg.port);
        allowed_hosts.extend(tailnet.host_headers(cfg.port));
        let updater = Updater::new(&ep.releases);
        Ok(Arc::new(Self {
            cfg,
            ep,
            store,
            refresher,
            usage,
            router,
            proxy_http: proxy_client(),
            control_http,
            logins: Logins::default(),
            allowed_hosts: RwLock::new(allowed_hosts),
            machines: Machines::default(),
            tailnet: RwLock::new(tailnet),
            hostname: tailnet::hostname(),
            paths: paths.clone(),
            hub_state,
            move_code: Mutex::new(None),
            ws_shutdown: tokio::sync::watch::channel(moving).0,
            management_key: management::load_or_create_key(&paths.management_key_file())?,
            api_call_http: management::api_call_client(),
            requests: Arc::new(RequestLog::default()),
            transcripts: Transcripts::open(&paths.transcripts_dir()?)?,
            lifecycle,
            updater,
            tailscale,
        }))
    }

    pub fn apply_tailnet(&self, tailnet: TailnetSelf) {
        let mut hosts = access::local_hosts(self.cfg.port);
        hosts.extend(tailnet.host_headers(self.cfg.port));
        *self.allowed_hosts.write().unwrap() = hosts;
        *self.tailnet.write().unwrap() = tailnet;
    }

    pub fn network_name(&self) -> String {
        self.tailnet
            .read()
            .unwrap()
            .short_name()
            .unwrap_or_else(|| self.hostname.clone())
    }

    fn serve_on(self: &Arc<Self>, listener: tokio::net::TcpListener) {
        let in_flight = self.lifecycle.in_flight();
        let stop = self.lifecycle.drain_started();
        let routes = self.routes().into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            let addr = listener.local_addr().ok();
            if let Err(e) = axum::serve(listener, routes).with_graceful_shutdown(stop).await {
                tracing::error!(?addr, "listener stopped: {e}");
            }
            drop(in_flight);
        });
    }

    async fn watch_tailnet(self: Arc<Self>, mut bound: HashSet<IpAddr>) {
        let mut warned = false;
        loop {
            let found = tokio::task::spawn_blocking(TailnetSelf::detect)
                .await
                .unwrap_or_default();
            if found.ips.is_empty() && !warned {
                tracing::warn!("Tailscale was not detected; the hub is only reachable from this machine for now");
                warned = true;
            }
            if found != *self.tailnet.read().unwrap() {
                self.apply_tailnet(found.clone());
            }
            for ip in found.ips {
                if bound.contains(&ip) {
                    continue;
                }
                let addr = SocketAddr::new(ip, self.cfg.port);
                match self.lifecycle.listen(addr) {
                    Ok(listener) => {
                        bound.insert(ip);
                        tracing::info!(%addr, "conflux hub listening on the tailnet");
                        self.serve_on(listener);
                    }
                    Err(e) => tracing::warn!(%addr, "could not bind the tailnet address yet: {e}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    pub fn accepting_requests(&self) -> bool {
        self.hub_state.get() == HubState::Active
    }

    pub fn redirect_hub(&self) -> Option<String> {
        match self.hub_state.get() {
            HubState::Retired { hub, .. } => Some(hub),
            _ => None,
        }
    }

    pub fn fleet(&self) -> Vec<Machine> {
        let latest = self.updater.latest();
        let hub = Machine {
            hostname: self.hostname.clone(),
            version: update::CURRENT.to_string(),
            role: "hub".into(),
            last_seen: chrono::Utc::now(),
            online: true,
            transcript_sync: Config::load(&self.paths).map(|c| c.transcript_sync).unwrap_or(false),
            update: self.updater.status(),
            outdated: false,
        };
        std::iter::once(hub)
            .chain(self.machines.list())
            .map(|mut m| {
                m.outdated = latest.as_deref().is_some_and(|l| update::is_newer(l, &m.version));
                m
            })
            .collect()
    }

    fn export_state(&self) -> (Value, mpsc::UnboundedReceiver<Value>) {
        let (successor, later) = mpsc::unbounded_channel();
        let state = json!({
            "router": self.router.export(successor.clone()),
            "usage": self.usage.export(),
            "requests": self.requests.export(successor),
            "machines": self.machines.export(),
            "latest": self.updater.latest(),
            "tailnet": *self.tailnet.read().unwrap(),
        });
        (state, later)
    }

    fn import_later(&self, change: &Value) {
        self.router.import_later_owner(&change["owner"]);
        self.requests.import_later(&change["request"]);
    }

    fn import_state(&self, state: &Value) {
        self.router.import(&state["router"]);
        self.usage.import(&state["usage"]);
        self.requests.import(&state["requests"]);
        self.machines.import(&state["machines"]);
        self.updater.set_latest(state["latest"].as_str().map(str::to_string));
    }

    pub fn routes(self: &Arc<Self>) -> axum::Router {
        let t3 = axum::Router::new()
            .route("/v0/management/auth-files", get(management::auth_files))
            .route("/v0/management/api-call", post(management::api_call))
            .route("/v0/management/reset-quota", post(management::reset_quota))
            .layer(axum::middleware::from_fn_with_state(
                self.clone(),
                management::require_key,
            ));
        axum::Router::new()
            .merge(t3)
            .route("/", get(dashboard::page))
            .route(
                "/conflux/transcripts",
                post(transcripts::upload).layer(DefaultBodyLimit::max(TRANSCRIPT_BODY_LIMIT)),
            )
            .route("/conflux/api/threads", get(transcripts::list))
            .route("/conflux/api/threads/{id}", get(transcripts::thread))
            .route("/conflux/api/transcripts/search", get(transcripts::search))
            .route("/conflux/api/hub/move-code", post(hubmove::create_code))
            .route("/conflux/api/hub/resume", post(hubmove::resume))
            .route("/conflux/move/prepare", post(hubmove::prepare))
            .route("/conflux/move/commit", post(hubmove::commit))
            .route("/conflux/move/abort", post(hubmove::abort))
            .route("/conflux/api/dashboard", get(dashboard::data))
            .route("/conflux/control", get(machines::control))
            .route("/conflux/api/update", post(api::update))
            .route("/conflux/api/discover", get(link::discover))
            .route("/conflux/api/link", post(link::link))
            .route("/conflux/api/status", get(api::status))
            .route("/conflux/api/login/claude/start", post(api::claude_start))
            .route("/conflux/api/login/claude/finish", post(api::claude_finish))
            .route("/conflux/api/login/codex/start", post(api::codex_start))
            .route("/conflux/api/login/{id}", get(api::login_status))
            .route(
                "/conflux/api/accounts/{id}",
                patch(api::update_account).delete(api::remove_account),
            )
            .route("/conflux/api/accounts/{id}/usage", post(api::poll_usage))
            .route("/v1/{*rest}", any(proxy::handle))
            .route("/backend-api/{*rest}", any(proxy::handle))
            .layer(axum::middleware::from_fn_with_state(self.clone(), hubmove::state_guard))
            .layer(axum::middleware::from_fn_with_state(self.clone(), access::guard))
            .with_state(self.clone())
    }

    pub fn start_background(self: &Arc<Self>) -> Vec<tokio::task::JoinHandle<()>> {
        vec![
            tokio::spawn(self.refresher.clone().run_scheduler()),
            tokio::spawn(self.usage.clone().run_poller(self.store.clone())),
            tokio::spawn(self.transcripts.clone().run_maintenance()),
        ]
    }
}

impl update::Handoff for App {
    async fn pause(&self) -> (Value, mpsc::UnboundedReceiver<Value>) {
        self.refresher.close_gate().await;
        self.usage.set_paused(true);
        self.export_state()
    }

    async fn resume(&self) {
        if self.hub_state.get() == HubState::Active {
            self.refresher.open_gate().await;
            self.usage.set_paused(false);
        }
    }
}

pub struct HubLock {
    _file: std::fs::File,
}

fn open_lock_file(paths: &Paths) -> Result<std::fs::File> {
    fsutil::ensure_private_dir(&paths.data_dir)?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.lock_file())
        .context("open hub lock")
}

pub fn lock_hub(paths: &Paths) -> Result<HubLock> {
    let file = open_lock_file(paths)?;
    if file.try_lock_exclusive().is_err() {
        bail!("another conflux hub is already running with this data directory");
    }
    Ok(HubLock { _file: file })
}

fn lock_hub_after_predecessor(paths: &Paths) -> Result<()> {
    let file = open_lock_file(paths)?;
    std::thread::spawn(move || {
        if file.lock_exclusive().is_ok() {
            std::mem::forget(file);
        }
    });
    Ok(())
}

pub async fn serve(cfg: Config, paths: Paths, ep: Endpoints) -> Result<()> {
    let lifecycle = Arc::new(Lifecycle::default());
    let takeover = lifecycle.take_over().await?;
    let _lock = match &takeover {
        None => Some(lock_hub(&paths)?),
        Some(_) => {
            lock_hub_after_predecessor(&paths)?;
            None
        }
    };
    let tailnet = takeover
        .as_ref()
        .and_then(|t| serde_json::from_value(t.state["tailnet"].clone()).ok())
        .unwrap_or_default();
    let addr = SocketAddr::from(([127, 0, 0, 1], cfg.port));
    let app = App::with_lifecycle(cfg, &paths, ep, tailnet, lifecycle.clone(), tailnet::system_tailscale())?;
    if let Some(takeover) = &takeover {
        app.import_state(&takeover.state);
    }
    let exe = update::running_exe()?;
    app.updater.replaces(exe.clone());
    let mut background = app.start_background();
    let listener = lifecycle.listen(addr).with_context(|| format!("bind {addr}"))?;
    app.serve_on(listener);
    let mut bound = HashSet::new();
    for listener in lifecycle.listen_inherited() {
        bound.extend(listener.local_addr().map(|a| a.ip()));
        app.serve_on(listener);
    }
    let local = format!("http://127.0.0.1:{}", addr.port());
    let sync = TranscriptSync::new(
        app.hostname.clone(),
        paths.clone(),
        &crate::setup::ClientPaths::from_env()?,
        Arc::new(move || local.clone()),
    );
    background.extend([
        tokio::spawn(app.clone().watch_tailnet(bound)),
        tokio::spawn(sync.run()),
        tokio::spawn(app.updater.clone().run_checks()),
    ]);
    tracing::info!(%addr, accounts = app.store.all().len(), "conflux hub listening");
    if let Some(takeover) = takeover {
        let app = app.clone();
        takeover.ready(move |change| app.import_later(&change)).await?;
    }
    update::remove_old_binaries(&exe);
    lifecycle.drain_started().await;
    tracing::info!("a newer conflux took over; finishing the requests already running");
    for task in background {
        task.abort();
    }
    lifecycle.drained().await;
    Ok(())
}

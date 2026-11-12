use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::config::{Config, Endpoints, Paths, Role};
use crate::setup::{self, ClientPaths};
use crate::tailnet::{self, TailnetSelf};
use crate::{app, hubmove, link, node, service, supervisor};

#[derive(Parser)]
#[command(
    name = "conflux",
    version,
    max_term_width = 100,
    after_help = "Account commands work from any machine. A node passes them to the hub.\n\
                  Run `conflux help <command>` for details.\n\
                  Docs: https://github.com/Byntham/conflux/blob/main/docs/commands.md"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(
        about = "Show each account's usage, cooldowns, and state",
        long_about = "Show each account's usage, cooldowns, and state.\n\n\
                      SHORT and LONG are the percentage used in the shortest and longest usage \
                      windows, for example 5 hours and 7 days."
    )]
    Status,
    #[command(
        about = "Add an account to the pool, or log one in again",
        long_about = "Add an account to the pool, or log one in again.\n\n\
                      claude prints a link. Approve access in a browser signed in to the account, \
                      then paste the URL from the address bar. The browser shows a connection \
                      error at that point, which is expected.\n\n\
                      codex prints a link and a code. Enter the code in a browser signed in to \
                      the ChatGPT account, and conflux finishes on its own."
    )]
    Login {
        #[arg(help = "Which kind of account to add")]
        provider: LoginProvider,
    },
    #[command(about = "Check an account's usage now and show it")]
    Usage {
        #[arg(help = "Account ID, as shown by `conflux status`")]
        account: String,
    },
    #[command(about = "Let conflux send requests to an account again")]
    Enable {
        #[arg(help = "Account ID, as shown by `conflux status`")]
        account: String,
    },
    #[command(
        about = "Stop using an account for new requests. Its login stays",
        long_about = "Stop using an account for new requests. Its login stays, and `conflux enable` brings it back."
    )]
    Disable {
        #[arg(help = "Account ID, as shown by `conflux status`")]
        account: String,
    },
    #[command(
        about = "Allow or forbid a Claude account's paid extra usage",
        long_about = "Allow or forbid a Claude account's paid extra usage.\n\n\
                      It is off by default. When on, conflux uses extra usage only when no other \
                      account has room."
    )]
    Overage {
        #[arg(help = "Account ID, as shown by `conflux status`")]
        account: String,
        setting: OnOff,
    },
    #[command(
        about = "Delete an account and its tokens from the hub",
        long_about = "Delete an account and its tokens from the hub. To use it again, run `conflux login`."
    )]
    Remove {
        #[arg(help = "Account ID, as shown by `conflux status`")]
        account: String,
    },
    #[command(
        about = "Make this machine the hub, or connect it to one",
        long_about = "Make this machine the hub, or connect it to one.\n\n\
                      Pass `self` to make this machine the hub. Pass another machine's tailnet \
                      name to make this machine a node of that hub.\n\n\
                      join installs conflux as a background service and points Claude Code \
                      (~/.claude/settings.json) and Codex (~/.codex/config.toml) at it. It saves \
                      the values it replaces, so `conflux off` and `conflux leave` can put them back."
    )]
    Join {
        #[arg(help = "`self` to make this machine the hub, or the hub's tailnet name")]
        hub: String,
        #[arg(long, help = "Do not install the background service. Run `conflux serve` yourself")]
        no_service: bool,
        #[arg(long, help = "Join another hub even though this machine holds accounts")]
        force: bool,
    },
    #[command(
        about = "Start conflux and wait to be linked from a hub's dashboard",
        long_about = "Install conflux as a background service and wait to be linked to a hub.\n\n\
                      On the hub's dashboard, open Machines and click Find machines, then Link. \
                      If this machine has already joined a hub or holds accounts, wait changes \
                      nothing. It only restarts the service if one is installed."
    )]
    Wait,
    #[command(
        about = "Point Claude Code and Codex back at this machine's own logins",
        long_about = "Point Claude Code and Codex back at this machine's own logins.\n\n\
                      Use it when the hub is down. The service keeps running. Restart open \
                      sessions to pick up the change. `conflux on` undoes it."
    )]
    Off,
    #[command(about = "Point Claude Code and Codex at conflux again after `conflux off`")]
    On,
    #[command(
        about = "Stop conflux and give Claude Code and Codex their old settings back",
        long_about = "Stop conflux and give Claude Code and Codex their old settings back.\n\n\
                      leave removes the background service. It keeps the binary, the config, and \
                      any account tokens on this machine. Run `conflux remove` first if you want \
                      the tokens gone."
    )]
    Leave,
    #[command(
        about = "Update machines to the latest release",
        long_about = "Update every online machine, or the ones you name, to the latest release.\n\n\
                      Each machine starts the new version next to the old one. The new version \
                      takes new requests, and the old one exits once its requests finish. The \
                      command waits up to 5 minutes and reports each machine."
    )]
    Update {
        #[arg(help = "Machine names as the dashboard shows them. Leave empty for every machine")]
        machines: Vec<String>,
    },
    #[command(
        about = "Turn transcript sync to the hub on or off for this machine",
        long_about = "Turn transcript sync to the hub on or off for this machine.\n\n\
                      When on, conflux copies this machine's Claude Code and Codex transcripts to \
                      the hub. Transcripts hold everything your agents have read."
    )]
    Sync { setting: SyncSetting },
    #[command(about = "Print the URL and key that T3 Code needs to show your pooled usage")]
    T3Key,
    #[command(about = "Move the hub to another machine")]
    Hub {
        #[command(subcommand)]
        action: HubAction,
    },
    #[command(
        about = "Run conflux in the foreground",
        long_about = "Run conflux in the foreground.\n\n\
                      The background service runs this command. Run it yourself after \
                      `conflux join <hub> --no-service`, or to watch logs while you debug. Set \
                      CONFLUX_LOG to change the log level."
    )]
    Serve,
    #[command(hide = true)]
    Worker,
}

#[derive(Subcommand)]
pub enum HubAction {
    #[command(
        about = "Print a one-time code for moving this hub. Run it on the current hub",
        long_about = "Print a one-time code for moving this hub. Run it on the current hub.\n\n\
                      It also prints the take-over command to run on the new machine. The code \
                      works for 10 minutes."
    )]
    MoveCode,
    #[command(
        about = "Make this machine the hub, taking the accounts from the old hub",
        long_about = "Make this machine the hub, taking the accounts from the old hub.\n\n\
                      Run it on the new machine. If a move stopped after the accounts reached \
                      this machine, run it again without --code to finish it."
    )]
    TakeOver {
        #[arg(help = "The current hub's tailnet name")]
        old_hub: String,
        #[arg(long, help = "The code from `conflux hub move-code`")]
        code: Option<String>,
        #[arg(long, help = "Allow a machine with a battery, such as a laptop, to become the hub")]
        force: bool,
    },
    #[command(
        about = "Put the old hub back in service after a move that did not finish",
        long_about = "Put the old hub back in service after a move that did not finish. Run it on the old hub."
    )]
    Resume,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum LoginProvider {
    #[value(help = "A Claude subscription, used by Claude Code")]
    Claude,
    #[value(help = "A ChatGPT subscription, used by Codex")]
    Codex,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SyncSetting {
    #[value(help = "Copy this machine's transcripts to the hub")]
    On,
    #[value(help = "Stop copying. Copies already on the hub stay")]
    Off,
    #[value(help = "Show whether sync is on")]
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

const UPDATE_WAIT: Duration = Duration::from_secs(300);

struct Daemon {
    base: String,
    http: reqwest::Client,
}

impl Daemon {
    fn new(cfg: &Config) -> Self {
        Self {
            base: format!("http://127.0.0.1:{}", cfg.port),
            http: reqwest::Client::new(),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(b) = body {
            req = req.json(&b);
        } else {
            req = req.header("content-type", "application/json");
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() {
                anyhow::anyhow!(
                    "the conflux daemon is not running on {}. Start it with `conflux serve`.",
                    self.base
                )
            } else {
                e.into()
            }
        })?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}", v["error"].as_str().unwrap_or(&format!("HTTP {status}")));
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
}

pub async fn run(cli: Cli) -> Result<()> {
    let paths = Paths::from_env()?;
    let cfg = Config::load(&paths)?;
    let daemon = Daemon::new(&cfg);
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        if let Some(url) = dashboard_url(&cfg) {
            println!("\nDashboard: {url}");
        }
        return Ok(());
    };
    match command {
        Command::Hub {
            action: HubAction::MoveCode,
        } => {
            let v = daemon.post("/conflux/api/hub/move-code", json!({})).await?;
            let code = v["code"].as_str().context("no code")?;
            let host = TailnetSelf::detect().short_name().unwrap_or_else(tailnet::hostname);
            println!("On the new hub machine, within 10 minutes, run:");
            println!("  conflux hub take-over {host} --code {code}");
            Ok(())
        }
        Command::Hub {
            action: HubAction::Resume,
        } => {
            daemon.post("/conflux/api/hub/resume", json!({})).await?;
            println!("The hub is back in service.");
            Ok(())
        }
        Command::Hub {
            action: HubAction::TakeOver { old_hub, code, force },
        } => take_over(cfg, paths, &old_hub, code, force).await,
        Command::Serve => tokio::task::spawn_blocking(supervisor::run).await?,
        Command::Update { machines } => update(&daemon, machines).await,
        Command::Worker => {
            if let Some((_, pending)) = hubmove::find_pending(&paths)? {
                tracing::warn!(old_hub = %pending.old_hub, "a hub move is unfinished. Run `conflux hub take-over` to finish it.");
            }
            loop {
                let cfg = Config::load(&paths)?;
                match cfg.role {
                    Role::Hub => return app::serve(cfg, paths, Endpoints::from_env()).await,
                    Role::Node => return node::serve(cfg, paths).await,
                    Role::Waiting => link::serve(cfg, paths.clone()).await?,
                }
            }
        }
        Command::Wait => match link::become_waiting(&paths)? {
            Some(role) => {
                let role = if role == Role::Node { "linked to a hub" } else { "a hub" };
                println!("This machine is already {role}. Its role did not change.");
                if service::installed() {
                    service::install()?;
                }
                Ok(())
            }
            None => {
                println!("Installed {}.", service::install()?);
                println!("conflux is running and waiting to be linked to a hub.");
                println!("On the hub's dashboard, open Machines and click Find machines.");
                Ok(())
            }
        },
        Command::Join { hub, no_service, force } => join(cfg, paths, &hub, no_service, force).await,
        Command::Sync { setting } => {
            let mut cfg = cfg;
            match setting {
                SyncSetting::On => {
                    cfg.transcript_sync = true;
                    cfg.save(&paths)?;
                    println!("Transcript sync is on for this machine.");
                    println!(
                        "Its Claude Code and Codex transcripts will be copied to the hub, including everything agents read."
                    );
                }
                SyncSetting::Off => {
                    cfg.transcript_sync = false;
                    cfg.save(&paths)?;
                    println!("Transcript sync is off for this machine. Copies already on the hub stay there.");
                }
                SyncSetting::Status => {
                    println!(
                        "Transcript sync is {} for this machine.",
                        if cfg.transcript_sync { "on" } else { "off" }
                    );
                }
            }
            Ok(())
        }
        Command::T3Key => {
            if cfg.role == Role::Node {
                bail!("run this on the hub ({})", cfg.hub.as_deref().unwrap_or("not set"));
            }
            let key = crate::management::load_or_create_key(&paths.management_key_file())?;
            let host = TailnetSelf::detect().short_name().unwrap_or_else(tailnet::hostname);
            println!("In T3 Code, open Settings > Providers > Usage providers > Add hub.");
            println!("URL: http://{host}:{}", cfg.port);
            println!("Management key: {key}");
            Ok(())
        }
        Command::Off => {
            let restored = setup::restore(&ClientPaths::from_env()?, &paths.config_dir, cfg.port)?;
            println!(
                "{}",
                if restored.is_some() {
                    "Claude Code and Codex now use this machine's own logins. Run `conflux on` to go back."
                } else {
                    "This machine has not joined a hub."
                }
            );
            Ok(())
        }
        Command::On => {
            setup::apply(&ClientPaths::from_env()?, &paths.config_dir, cfg.port)?;
            println!("Claude Code and Codex now go through conflux.");
            Ok(())
        }
        Command::Leave => {
            setup::restore(&ClientPaths::from_env()?, &paths.config_dir, cfg.port)?;
            service::uninstall()?;
            crate::fsutil::remove_durable(&setup::joined_path(&paths.config_dir))?;
            println!("conflux is stopped and removed from Claude Code and Codex. Account data was left in place.");
            Ok(())
        }
        Command::Login {
            provider: LoginProvider::Claude,
        } => login_claude(&daemon).await,
        Command::Login {
            provider: LoginProvider::Codex,
        } => login_codex(&daemon).await,
        Command::Status => print_status(&daemon.get("/conflux/api/status").await?),
        Command::Enable { account } => patch(&daemon, &account, json!({ "enabled": true })).await,
        Command::Disable { account } => patch(&daemon, &account, json!({ "enabled": false })).await,
        Command::Overage { account, setting } => {
            patch(
                &daemon,
                &account,
                json!({ "allow_overage": matches!(setting, OnOff::On) }),
            )
            .await
        }
        Command::Remove { account } => {
            daemon
                .call(
                    reqwest::Method::DELETE,
                    &format!("/conflux/api/accounts/{account}"),
                    None,
                )
                .await?;
            println!("Removed {account}.");
            Ok(())
        }
        Command::Usage { account } => {
            let v = daemon
                .post(&format!("/conflux/api/accounts/{account}/usage"), json!({}))
                .await?;
            print_status(&json!({ "accounts": [v] }))
        }
    }
}

fn dashboard_url(cfg: &Config) -> Option<String> {
    let host = match cfg.role {
        Role::Hub => {
            let me = TailnetSelf::detect();
            me.dns_name
                .clone()
                .or_else(|| me.short_name())
                .unwrap_or_else(|| "127.0.0.1".into())
        }
        Role::Node => cfg.hub.clone()?,
        Role::Waiting => return None,
    };
    Some(format!("http://{host}:{}", cfg.port))
}

fn is_this_machine(hub: &str) -> bool {
    let hub = hub.to_lowercase();
    let t = TailnetSelf::detect();
    ["self", "localhost", "127.0.0.1"].contains(&hub.as_str())
        || hub == tailnet::hostname()
        || t.names().contains(&hub)
        || t.ips.iter().any(|ip| ip.to_string() == hub)
}

async fn commit_move(http: &reqwest::Client, pending: &hubmove::Pending, me: &str) -> Result<bool> {
    let url = format!("http://{}:{}/conflux/move/commit", pending.old_hub, pending.port);
    for attempt in 0..30 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let sent = http
            .post(&url)
            .json(&json!({ "id": pending.id, "hub": me }))
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
    bail!(
        "could not confirm the move with {}. Run `conflux hub take-over {}` again to retry.",
        pending.old_hub,
        pending.old_hub
    )
}

async fn take_over(mut cfg: Config, paths: Paths, old_hub: &str, code: Option<String>, force: bool) -> Result<()> {
    let me = TailnetSelf::detect()
        .short_name()
        .context("Tailscale must be running on this machine")?;
    if hubmove::is_laptop() && !force {
        bail!(
            "this machine has a battery, so it may leave home and change IP. Pass --force to make it the hub anyway."
        );
    }
    let http = reqwest::Client::new();
    let (dir, pending) = match hubmove::find_pending(&paths)? {
        Some(found) => {
            println!("Resuming the move from {}.", found.1.old_hub);
            found
        }
        None => {
            if paths.has_accounts() {
                bail!("this machine already holds accounts");
            }
            let code = code.context("pass --code with the code from `conflux hub move-code`")?;
            let id = uuid::Uuid::new_v4().to_string();
            let base = format!("http://{old_hub}:{}", cfg.port);
            println!(
                "Asking {old_hub} to stop and export its accounts. Codex WebSocket sessions close now. HTTP responses running through this machine end when it becomes the hub."
            );
            let resp = http
                .post(format!("{base}/conflux/move/prepare"))
                .json(&json!({ "code": code, "id": id }))
                .timeout(Duration::from_secs(300))
                .send()
                .await
                .with_context(|| format!("reach the hub at {old_hub}"))?;
            if !resp.status().is_success() {
                let v: Value = resp.json().await.unwrap_or(Value::Null);
                bail!("{}", v["error"].as_str().unwrap_or("the hub refused the move"));
            }
            let bundle: hubmove::Bundle = resp.json().await.context("read the account export")?;
            match hubmove::import_bundle(&paths, &bundle, old_hub, cfg.port) {
                Ok(imported) => imported,
                Err(e) => {
                    let _ = http
                        .post(format!("{base}/conflux/move/abort"))
                        .json(&json!({ "id": id }))
                        .send()
                        .await;
                    return Err(e.context("store the exported accounts"));
                }
            }
        }
    };

    if !commit_move(&http, &pending, &me).await? {
        hubmove::discard_incoming(&dir)?;
        bail!("the move was cancelled on {}. Nothing changed here.", pending.old_hub);
    }

    hubmove::activate(&paths, &dir, &pending)?;
    cfg.role = Role::Hub;
    cfg.hub = None;
    cfg.save(&paths)?;
    crate::fsutil::remove_durable(&hubmove::hub_state_file(&paths))?;
    hubmove::discard_incoming(&dir)?;
    if service::installed() {
        service::restart()?;
    } else {
        println!("Installed {}.", service::install()?);
    }
    setup::apply(&ClientPaths::from_env()?, &paths.config_dir, cfg.port)?;
    println!("{me} is now the hub. Other machines will follow within a minute.");
    println!(
        "On {}, run `conflux join {me}` so that machine routes through the new hub.",
        pending.old_hub
    );
    Ok(())
}

async fn join(mut cfg: Config, paths: Paths, hub: &str, no_service: bool, force: bool) -> Result<()> {
    let state_file = hubmove::hub_state_file(&paths);
    match crate::hubstate::HubStateStore::load(&state_file)?.get() {
        crate::hubstate::HubState::Moving { .. } => {
            bail!("this hub is in the middle of a move. Finish it, or run `conflux hub resume`.")
        }
        _ => crate::fsutil::remove_durable(&state_file)?,
    }
    let becoming_hub = is_this_machine(hub);
    if !becoming_hub && paths.has_accounts() && !force {
        bail!(
            "this machine holds logged-in accounts. Joining another hub would strand them. \
             Move them with `conflux hub take-over` on the new hub, or pass --force."
        );
    }
    cfg.role = if becoming_hub { Role::Hub } else { Role::Node };
    cfg.hub = (!becoming_hub).then(|| hub.to_string());
    cfg.save(&paths)?;

    if !becoming_hub {
        let url = format!("http://{hub}:{}/conflux/api/status", cfg.port);
        let reachable = reqwest::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        match reachable {
            Ok(r) if r.status().is_success() => println!("Reached the hub at {hub}."),
            Ok(r) => println!("Warning: the hub at {hub} answered HTTP {}.", r.status()),
            Err(_) => println!(
                "Warning: could not reach a conflux hub at {hub}:{}. Is it running?",
                cfg.port
            ),
        }
    }

    if no_service {
        println!("Skipped the service. Start the daemon with `conflux serve`.");
    } else {
        println!("Installed {}.", service::install()?);
    }

    setup::apply(&ClientPaths::from_env()?, &paths.config_dir, cfg.port)?;
    for warning in setup::env_warnings() {
        println!("Warning: {warning}");
    }
    if becoming_hub {
        println!("This machine is now the hub. Add accounts with `conflux login claude` and `conflux login codex`.");
    } else {
        println!("Claude Code and Codex on this machine now go through {hub}.");
    }
    println!("New or restarted Claude Code and Codex sessions pick this up. Running ones keep their old settings.");
    if let Some(url) = dashboard_url(&cfg) {
        println!("\nDashboard: {url}");
    }
    Ok(())
}

async fn update(daemon: &Daemon, machines: Vec<String>) -> Result<()> {
    let only = (!machines.is_empty()).then_some(machines);
    let asked = daemon.post("/conflux/api/update", json!({ "machines": only })).await?;
    let version = asked["version"].as_str().context("no version")?.to_string();
    for s in asked["skipped"].as_array().into_iter().flatten() {
        if only.is_some() || s["reason"] != "already up to date" {
            println!(
                "{}: skipped, {}",
                s["machine"].as_str().unwrap_or("?"),
                s["reason"].as_str().unwrap_or("?")
            );
        }
    }
    let mut waiting: Vec<String> = asked["started"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(str::to_string))
        .collect();
    let unknown = asked["skipped"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["reason"] == "unknown machine");
    if waiting.is_empty() {
        if unknown {
            bail!("the hub does not know every machine you named");
        }
        println!("Nothing to update. The latest release is v{version}.");
        return Ok(());
    }
    println!("Updating {} to v{version}.", waiting.join(", "));
    let deadline = std::time::Instant::now() + UPDATE_WAIT;
    let mut failed = unknown;
    while !waiting.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(dashboard) = daemon.get("/conflux/api/dashboard").await else {
            continue;
        };
        for m in dashboard["machines"].as_array().into_iter().flatten() {
            let Some(name) = m["hostname"].as_str().filter(|n| waiting.iter().any(|w| w == n)) else {
                continue;
            };
            if m["update"]["state"] == "failed" {
                println!("{name}: failed, {}", m["update"]["error"].as_str().unwrap_or("?"));
                failed = true;
            } else if m["version"] == version.as_str() && m["online"] == true {
                println!("{name}: now on v{version}");
            } else {
                continue;
            }
            waiting.retain(|w| w != name);
        }
    }
    for name in &waiting {
        println!("{name}: still updating. Check the dashboard.");
    }
    if failed || !waiting.is_empty() {
        bail!("not every machine updated");
    }
    Ok(())
}

async fn patch(daemon: &Daemon, account: &str, body: Value) -> Result<()> {
    let v = daemon
        .call(
            reqwest::Method::PATCH,
            &format!("/conflux/api/accounts/{account}"),
            Some(body),
        )
        .await?;
    print_status(&json!({ "accounts": [v] }))
}

async fn login_claude(daemon: &Daemon) -> Result<()> {
    let start = daemon.post("/conflux/api/login/claude/start", json!({})).await?;
    let url = start["url"].as_str().context("no url")?;
    let login_id = start["login_id"].as_str().context("no login_id")?;
    println!("1. Open this URL in a browser signed in to the Claude account:\n\n   {url}\n");
    println!("2. Approve access. The browser then shows a connection error. That is expected.");
    print!("3. Paste the whole URL from the address bar here: ");
    std::io::stdout().flush()?;
    let mut code = String::new();
    std::io::stdin().lock().read_line(&mut code)?;
    let done = daemon
        .post(
            "/conflux/api/login/claude/finish",
            json!({ "login_id": login_id, "code": code.trim() }),
        )
        .await?;
    println!(
        "\nLogged in {} as account {}.",
        done["email"].as_str().unwrap_or("(no email)"),
        done["account"].as_str().unwrap_or("?")
    );
    Ok(())
}

async fn login_codex(daemon: &Daemon) -> Result<()> {
    let start = daemon.post("/conflux/api/login/codex/start", json!({})).await?;
    let login_id = start["login_id"].as_str().context("no login_id")?.to_string();
    println!(
        "1. Open {} in a browser signed in to the ChatGPT account.",
        start["status"]["verify_url"].as_str().unwrap_or("")
    );
    println!(
        "2. Enter this code: {}\n",
        start["status"]["user_code"].as_str().unwrap_or("")
    );
    println!("Waiting for approval...");
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let s = daemon.get(&format!("/conflux/api/login/{login_id}")).await?;
        match s["state"].as_str() {
            Some("pending") => continue,
            Some("done") => {
                println!(
                    "Logged in {} as account {}.",
                    s["email"].as_str().unwrap_or("(no email)"),
                    s["account"].as_str().unwrap_or("?")
                );
                return Ok(());
            }
            _ => bail!("login failed: {}", s["error"].as_str().unwrap_or("unknown error")),
        }
    }
}

fn pct(v: &Value) -> String {
    v.as_f64().map(|p| format!("{p:.0}%")).unwrap_or_else(|| "-".into())
}

fn print_status(v: &Value) -> Result<()> {
    if v["role"] == "waiting" {
        println!("This machine is waiting to be linked to a hub.");
        println!("On the hub's dashboard, open Machines and click Find machines. Or run `conflux join <hub>` here.");
        return Ok(());
    }
    let accounts = v["accounts"].as_array().cloned().unwrap_or_default();
    if accounts.is_empty() {
        println!("No accounts yet. Add one with `conflux login claude` or `conflux login codex`.");
        return Ok(());
    }
    println!(
        "{:<20} {:<7} {:<32} {:<6} {:<6} STATUS",
        "ACCOUNT", "TYPE", "EMAIL", "SHORT", "LONG"
    );
    for a in accounts {
        let windows = a["usage"]["windows"].as_array().cloned().unwrap_or_default();
        let shared: Vec<_> = windows.iter().filter(|w| w["model_scope"].is_null()).collect();
        let short = shared
            .iter()
            .min_by_key(|w| w["duration_secs"].as_i64().unwrap_or(i64::MAX));
        let long = shared.iter().max_by_key(|w| w["duration_secs"].as_i64().unwrap_or(0));
        let mut status = Vec::new();
        if a["enabled"] == false {
            status.push("disabled".to_string());
        }
        if let Some(r) = a["needs_login"].as_str() {
            status.push(format!("needs login ({r})"));
        }
        if let Some(r) = a["billing_blocked"].as_str() {
            status.push(format!("billing blocked ({r})"));
        }
        for c in a["cooldowns"].as_array().into_iter().flatten() {
            status.push(format!(
                "cooling {} until {}",
                c["scope"].as_str().unwrap_or(""),
                c["until"].as_str().unwrap_or("")
            ));
        }
        if a["allow_overage"] == true {
            status.push("overage allowed".into());
        }
        if status.is_empty() {
            status.push("ok".into());
        }
        println!(
            "{:<20} {:<7} {:<32} {:<6} {:<6} {}",
            a["id"].as_str().unwrap_or(""),
            a["provider"].as_str().unwrap_or(""),
            a["email"].as_str().unwrap_or("-"),
            short.map(|w| pct(&w["used_pct"])).unwrap_or_else(|| "-".into()),
            long.map(|w| pct(&w["used_pct"])).unwrap_or_else(|| "-".into()),
            status.join(", ")
        );
    }
    Ok(())
}

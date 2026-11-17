mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::routing::get;
use common::Harness;
use conflux::accounts::{Provider, Receipt, RefreshState};
use conflux::app::App;
use conflux::config::{Config, Endpoints, Paths, Role};
use conflux::hubmove::{self, Bundle};
use conflux::hubstate::{HubState, HubStateStore};
use conflux::node::Node;
use conflux::refresh::RefreshResult;
use conflux::setup::ClientPaths;
use conflux::tailnet::Tailscale;
use conflux::update;
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ID: &str = "22222222-2222-4222-8222-222222222222";
const NEW_HUB: &str = "[::1]";
const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn whois(user: u64) -> Value {
    json!({
        "Node": { "StableID": "n-one", "Name": format!("{NEW_HUB}."), "Tags": [] },
        "UserProfile": { "ID": user, "LoginName": "someone@example.com" },
    })
}

struct Hub {
    h: Harness,
    caller: Arc<Mutex<Value>>,
}

impl std::ops::Deref for Hub {
    type Target = Harness;
    fn deref(&self) -> &Harness {
        &self.h
    }
}

fn tailscale(caller: Arc<Mutex<Value>>) -> Tailscale {
    common::fake_tailscale(Arc::new(Mutex::new(json!({ "Self": { "UserID": 1 } }))), caller)
}

fn hub_run_by(user: u64) -> Tailscale {
    tailscale(Arc::new(Mutex::new(whois(user))))
}

async fn hub_on(listener: tokio::net::TcpListener) -> Hub {
    let caller = Arc::new(Mutex::new(whois(1)));
    Hub {
        h: Harness::on_tailnet(listener, tailscale(caller.clone()), "false").await,
        caller,
    }
}

async fn hub() -> Hub {
    hub_on(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()).await
}

async fn post(h: &Harness, path: &str, body: Value) -> reqwest::Response {
    h.http
        .post(format!("{}{path}", h.base))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(found) = check() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the expected state never arrived");
}

async fn fake_node(h: &Harness, report: Value) -> Socket {
    let url = format!("ws://127.0.0.1:{}/conflux/control", h.port);
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws.send(Message::Text(report.to_string().into())).await.unwrap();
    let name = report["hostname"].as_str().unwrap().to_string();
    eventually(|| {
        h.app
            .machines
            .list()
            .into_iter()
            .find(|m| m.hostname == name && m.online)
    })
    .await;
    ws
}

fn report() -> Value {
    json!({ "hostname": "node-one", "version": update::CURRENT, "has_battery": false })
}

async fn start_move(h: &Harness, machine: &str, force: bool) -> reqwest::Response {
    post(
        h,
        "/conflux/api/hub/move",
        json!({ "machine": machine, "force": force }),
    )
    .await
}

async fn become_hub_command(ws: &mut Socket) -> Value {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("the hub sends a command")
            .unwrap()
            .unwrap();
        if let Message::Text(text) = frame {
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            if v["type"] == "become_hub" {
                return v;
            }
        }
    }
}

async fn moving(h: &Harness) -> (Socket, Bundle) {
    let mut ws = fake_node(h, report()).await;
    let r = start_move(h, "node-one", false).await;
    assert_eq!(r.status(), 200);
    let command = become_hub_command(&mut ws).await;
    (ws, serde_json::from_value(command).unwrap())
}

async fn commit(h: &Harness, id: &str) -> Value {
    post(h, "/conflux/move/commit", json!({ "id": id }))
        .await
        .json()
        .await
        .unwrap()
}

fn paths(h: &Harness) -> Paths {
    Paths::under(h.dir.path())
}

fn login(name: &str) -> conflux::oauth::LoginTokens {
    conflux::oauth::LoginTokens {
        provider: Provider::Claude,
        identity: name.into(),
        email: None,
        plan: None,
        chatgpt_account_id: None,
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        id_token: None,
    }
}

async fn serve_new_hub(port: u16) {
    let listener = tokio::net::TcpListener::bind(("::1", port)).await.unwrap();
    let routes = axum::Router::new().route("/conflux/api/status", get(|| async { Json(json!({ "role": "hub" })) }));
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
}

#[tokio::test]
async fn a_move_sends_the_accounts_to_the_target_and_stops_service() {
    let h = hub().await;
    let id = h.add(Provider::Claude, "a").await;
    let e = h.app.store.get(&id).unwrap();
    e.journal.write_claim(2).unwrap();
    e.journal.write_receipt(2, &Receipt::Unknown).unwrap();

    let (_ws, bundle) = moving(&h).await;
    assert!(bundle.files.iter().any(|f| f.path == format!("{id}.json")));
    assert!(bundle.files.iter().any(|f| f.path.ends_with("receipt-2")));
    assert_eq!(bundle.management_key, h.app.management_key);
    assert_eq!(
        h.app.hub_state.get(),
        HubState::Moving {
            id: bundle.id.clone(),
            hub: NEW_HUB.into(),
            address: Some(LOCAL),
        }
    );

    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("moving"));
    assert_eq!(h.hits_for("at-a"), 0);

    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::GateClosed);
    assert!(h.app.refresher.install_login(login("b")).await.is_err());
}

#[tokio::test]
async fn the_hub_refuses_a_target_that_cannot_take_over_without_touching_state() {
    let h = hub().await;
    h.add(Provider::Codex, "c").await;
    let refused = |r: reqwest::Response, status: StatusCode| async move {
        assert_eq!(r.status(), status);
        let v: Value = r.json().await.unwrap();
        v["error"].as_str().unwrap().to_string()
    };

    assert_eq!(start_move(&h, "nowhere", false).await.status(), StatusCode::NOT_FOUND);

    *h.caller.lock().unwrap() = whois(2);
    h.app.forget_identities();
    let _guest = fake_node(&h, json!({ "hostname": "theirs", "version": update::CURRENT })).await;
    *h.caller.lock().unwrap() = whois(1);
    h.app.forget_identities();
    let message = refused(start_move(&h, "theirs", true).await, StatusCode::FORBIDDEN).await;
    assert!(message.contains("guest"), "{message}");

    let _old = fake_node(&h, json!({ "hostname": "old", "version": "0.0.1" })).await;
    let message = refused(start_move(&h, "old", true).await, StatusCode::CONFLICT).await;
    assert!(message.contains("v0.0.1"), "{message}");

    let mut laptop = fake_node(
        &h,
        json!({ "hostname": "laptop", "version": update::CURRENT, "has_battery": true }),
    )
    .await;
    let message = refused(start_move(&h, "laptop", false).await, StatusCode::CONFLICT).await;
    assert!(message.contains("battery"), "{message}");

    let mut req = format!("ws://127.0.0.1:{}/v1/responses", h.port)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("user-agent", "codex_cli_rs/0.200.0".parse().unwrap());
    let (mut codex, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    codex
        .send(Message::Text(
            json!({"type": "response.create", "model": "m"}).to_string().into(),
        ))
        .await
        .unwrap();
    for _ in 0..3 {
        codex.next().await.unwrap().unwrap();
    }
    assert_eq!(h.app.hub_state.get(), HubState::Active);
    codex
        .send(Message::Text(
            json!({"type": "response.create", "model": "m"}).to_string().into(),
        ))
        .await
        .unwrap();
    assert!(
        matches!(codex.next().await, Some(Ok(Message::Text(_)))),
        "refusals leave sockets open"
    );

    assert_eq!(start_move(&h, "laptop", true).await.status(), 200);
    become_hub_command(&mut laptop).await;
    let next = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match codex.next().await {
                Some(Ok(Message::Text(_))) => continue,
                other => return other,
            }
        }
    })
    .await
    .expect("socket closes once the move starts");
    assert!(matches!(next, None | Some(Ok(Message::Close(_))) | Some(Err(_))));
}

#[tokio::test]
async fn only_the_owner_can_start_a_move() {
    let h = hub().await;
    let _node = fake_node(&h, report()).await;
    *h.caller.lock().unwrap() = whois(2);
    h.app.forget_identities();
    assert_eq!(start_move(&h, "node-one", false).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.app.hub_state.get(), HubState::Active);
}

#[tokio::test]
async fn commit_retires_the_store_and_is_idempotent() {
    let h = hub().await;
    h.add(Provider::Claude, "a").await;
    let (_ws, bundle) = moving(&h).await;
    assert_eq!(commit(&h, &bundle.id).await["status"], "ok");
    let p = paths(&h);
    assert!(!p.accounts_dir().exists());
    assert!(hubmove::retired_dir(&p, &bundle.id).exists());
    assert_eq!(
        commit(&h, &bundle.id).await["status"],
        "ok",
        "a lost reply can be retried"
    );

    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains(NEW_HUB));

    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = no_redirect.get(format!("{}/", h.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
    assert!(r.headers()["location"].to_str().unwrap().contains(NEW_HUB));

    assert_eq!(
        post(&h, "/conflux/move/abort", json!({"id": bundle.id})).await.status(),
        StatusCode::CONFLICT
    );
    assert!(matches!(h.app.hub_state.get(), HubState::Retired { .. }));
}

#[tokio::test]
async fn only_the_target_can_commit_or_abort() {
    let h = hub().await;
    let other = tokio::net::TcpListener::bind(("::1", h.port)).await.unwrap();
    let other_base = format!("http://{}", other.local_addr().unwrap());
    let routes = h.app.routes().into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(other, routes).await.unwrap() });
    let (_ws, bundle) = moving(&h).await;
    let from_elsewhere = |action: &'static str| {
        h.http
            .post(format!("{other_base}/conflux/move/{action}"))
            .json(&json!({ "id": bundle.id }))
            .send()
    };
    for action in ["commit", "abort"] {
        let status = from_elsewhere(action).await.unwrap().status();
        assert_eq!(status, StatusCode::FORBIDDEN, "{action}");
    }
    assert!(matches!(h.app.hub_state.get(), HubState::Moving { .. }));
    assert!(paths(&h).accounts_dir().exists());

    assert_eq!(commit(&h, &bundle.id).await["status"], "ok");
    let status = from_elsewhere("commit").await.unwrap().status();
    assert_eq!(status, StatusCode::FORBIDDEN, "a retired hub still checks who retries");
}

#[tokio::test]
async fn abort_returns_to_service() {
    let h = hub().await;
    let id = h.add(Provider::Claude, "a").await;
    let (_ws, bundle) = moving(&h).await;
    assert_eq!(
        post(&h, "/conflux/move/abort", json!({"id": bundle.id})).await.status(),
        200
    );
    assert_eq!(h.app.hub_state.get(), HubState::Active);
    assert_eq!(h.claude(json!({})).send().await.unwrap().status(), 200);
    assert_eq!(
        commit(&h, &bundle.id).await["status"],
        "aborted",
        "a late commit must not touch a serving hub"
    );
    assert!(paths(&h).accounts_dir().exists());
    h.fake.refresh_replies.lock().unwrap().push_back(common::Reply::Json(
        200,
        vec![],
        json!({"access_token": "n", "refresh_token": "m", "expires_in": 100}).to_string(),
    ));
    let e = h.app.store.get(&id).unwrap();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Refreshed);
}

#[tokio::test]
async fn resume_works_from_moving_only() {
    let status = Arc::new(Mutex::new(json!({ "Self": { "UserID": 1 } })));
    let caller = Arc::new(Mutex::new(whois(1)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = Harness::on_tailnet(
        listener,
        common::fake_tailscale(status.clone(), caller.clone()),
        "false",
    )
    .await;
    assert_eq!(
        post(&h, "/conflux/api/hub/resume", json!({})).await.status(),
        StatusCode::CONFLICT
    );
    moving(&h).await;
    *caller.lock().unwrap() = whois(2);
    h.app.forget_identities();
    assert_eq!(
        post(&h, "/conflux/api/hub/resume", json!({})).await.status(),
        StatusCode::FORBIDDEN
    );
    *status.lock().unwrap() = Value::Null;
    h.app.forget_identities();
    assert_eq!(
        post(&h, "/conflux/api/hub/resume", json!({})).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    *status.lock().unwrap() = json!({ "Self": { "UserID": 1 } });
    *caller.lock().unwrap() = Value::Null;
    h.app.forget_identities();
    assert_eq!(post(&h, "/conflux/api/hub/resume", json!({})).await.status(), 200);
    assert_eq!(h.app.hub_state.get(), HubState::Active);
}

#[tokio::test]
async fn a_move_that_does_not_finish_in_time_resumes_the_hub() {
    let h = hub().await;
    h.add(Provider::Claude, "a").await;
    let (_ws, bundle) = moving(&h).await;
    hubmove::resume_after(h.app.clone(), bundle.id.clone(), Duration::from_millis(50)).await;
    assert_eq!(h.app.hub_state.get(), HubState::Active);
    assert_eq!(h.claude(json!({})).send().await.unwrap().status(), 200);
    assert_eq!(commit(&h, &bundle.id).await["status"], "aborted");
}

#[tokio::test]
async fn commit_with_another_id_is_refused() {
    let h = hub().await;
    moving(&h).await;
    assert_eq!(
        commit(&h, "33333333-3333-4333-8333-333333333333").await["status"],
        "aborted"
    );
    assert!(matches!(h.app.hub_state.get(), HubState::Moving { .. }));
    assert!(paths(&h).accounts_dir().exists());
}

#[tokio::test]
async fn commit_and_resume_racing_never_leave_both() {
    for _ in 0..8 {
        let h = Arc::new(hub().await);
        let (_ws, bundle) = moving(&h).await;
        let (a, b) = (h.clone(), h.clone());
        let id = bundle.id.clone();
        let commit_task = tokio::spawn(async move { commit(&a, &id).await });
        let resume_task = tokio::spawn(async move { post(&b, "/conflux/api/hub/resume", json!({})).await.status() });
        let committed = commit_task.await.unwrap()["status"] == "ok";
        let resumed = resume_task.await.unwrap() == StatusCode::OK;
        assert!(committed ^ resumed, "committed={committed} resumed={resumed}");
        match h.app.hub_state.get() {
            HubState::Retired { .. } => assert!(committed),
            HubState::Active => assert!(resumed),
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn a_running_stream_survives_the_old_hub_becoming_a_node() {
    let h = hub().await;
    h.add(Provider::Claude, "a").await;
    h.reply("at-a", common::Reply::Endless);
    let mut stream = h.claude(json!({})).send().await.unwrap();
    assert!(stream.chunk().await.unwrap().is_some());

    let (_ws, bundle) = tokio::time::timeout(Duration::from_secs(5), moving(&h))
        .await
        .expect("a move does not wait for running streams");
    serve_new_hub(h.port).await;
    assert_eq!(commit(&h, &bundle.id).await["status"], "ok");

    let p = paths(&h);
    eventually(|| h.app.lifecycle.is_draining().then_some(())).await;
    let cfg = Config::load(&p).unwrap();
    assert_eq!(cfg.role, Role::Node);
    assert_eq!(cfg.hub.as_deref(), Some(NEW_HUB));
    assert!(!hubmove::hub_state_file(&p).exists());

    assert!(!h.fake.stream_dropped.load(Ordering::SeqCst));
    assert!(stream.chunk().await.unwrap().is_some());
    drop(stream);
    eventually(|| h.fake.stream_dropped.load(Ordering::SeqCst).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !p.accounts_dir().exists(),
        "the finished stream must not bring the accounts back"
    );
}

#[tokio::test]
async fn a_node_takes_over_the_accounts_and_becomes_the_hub() {
    let h = hub().await;
    let id = h.add(Provider::Claude, "a").await;
    let e = h.app.store.get(&id).unwrap();
    e.journal.write_claim(2).unwrap();
    let at = chrono::Utc::now() + chrono::Duration::minutes(5);
    e.journal
        .write_receipt(
            2,
            &Receipt::Retry {
                next_at: at,
                retries: 1,
            },
        )
        .unwrap();
    let guest = conflux::guests::Guest {
        login: "friend@example.com".into(),
        hostname: "guest-box".into(),
        approved: true,
        ask: "x".into(),
    };
    h.app.guests.set("nGuest", guest).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let new_paths = Paths::under(dir.path());
    Config {
        role: Role::Node,
        hub: Some("127.0.0.1".into()),
        ..Config::default()
    }
    .save(&new_paths)
    .unwrap();
    let clients = ClientPaths {
        claude_settings: dir.path().join("claude/settings.json"),
        codex_config: dir.path().join("codex/config.toml"),
        grok_config: dir.path().join("grok/config.toml"),
    };
    let node = Node::named(
        "node-one".into(),
        "127.0.0.1".into(),
        h.port,
        0,
        new_paths.clone(),
        clients.clone(),
        hub_run_by(1),
    );
    tokio::spawn(node.clone().run_control());
    eventually(|| h.app.machines.list().into_iter().find(|m| m.online)).await;
    assert_eq!(start_move(&h, "node-one", true).await.status(), 200);

    eventually(|| node.lifecycle.is_draining().then_some(())).await;
    assert!(matches!(h.app.hub_state.get(), HubState::Retired { .. }));
    let cfg = Config::load(&new_paths).unwrap();
    assert_eq!(cfg.role, Role::Hub);
    assert_eq!(cfg.hub, None);
    assert!(hubmove::find_pending(&new_paths).unwrap().is_none());
    assert!(
        !clients.claude_settings.exists(),
        "the move leaves client settings alone"
    );

    let new_app = App::new(Config::default(), &new_paths, Endpoints::all_at("http://127.0.0.1:1")).unwrap();
    let moved = new_app.store.get(&id).unwrap();
    assert_eq!(moved.credentials().grant.refresh_token, "rt-a");
    assert!(matches!(
        moved.refresh_state(),
        RefreshState::RetryAt { retries: 1, .. }
    ));
    assert_eq!(new_app.management_key, h.app.management_key);
    assert!(new_app.guests.is_approved("nGuest"));
    assert_eq!(new_app.hub_state.get(), HubState::Active);
}

#[tokio::test]
async fn a_node_refuses_to_become_the_hub_it_cannot_safely_be() {
    for (holds_accounts, hub_user) in [(true, 1), (false, 2)] {
        let h = hub().await;
        h.add(Provider::Claude, "a").await;
        let dir = tempfile::tempdir().unwrap();
        let new_paths = Paths::under(dir.path());
        if holds_accounts {
            conflux::fsutil::ensure_private_dir(&new_paths.accounts_dir()).unwrap();
            std::fs::write(new_paths.accounts_dir().join("mine.json"), "{}").unwrap();
        }
        let node = Node::named(
            "node-one".into(),
            "127.0.0.1".into(),
            h.port,
            0,
            new_paths.clone(),
            ClientPaths {
                claude_settings: dir.path().join("claude/settings.json"),
                codex_config: dir.path().join("codex/config.toml"),
                grok_config: dir.path().join("grok/config.toml"),
            },
            hub_run_by(hub_user),
        );
        tokio::spawn(node.clone().run_control());
        eventually(|| h.app.machines.list().into_iter().find(|m| m.online)).await;
        assert_eq!(start_move(&h, "node-one", true).await.status(), 200);
        eventually(|| (h.app.hub_state.get() == HubState::Active).then_some(())).await;
        assert!(hubmove::find_pending(&new_paths).unwrap().is_none());
        assert_eq!(new_paths.has_accounts(), holds_accounts);
        assert!(!node.lifecycle.is_draining());
    }
}

fn set_state(p: &Paths, state: HubState) {
    let s = HubStateStore::load(&hubmove::hub_state_file(p)).unwrap();
    assert!(s.compare_and_set(&HubState::Active, state).unwrap());
}

fn app_at(p: &Paths, port: u16) -> Arc<App> {
    let cfg = Config {
        port,
        ..Config::default()
    };
    App::new(cfg, p, Endpoints::all_at("http://127.0.0.1:1")).unwrap()
}

async fn free_port() -> u16 {
    let probe = tokio::net::TcpListener::bind(("::1", 0)).await.unwrap();
    probe.local_addr().unwrap().port()
}

#[tokio::test]
async fn restart_while_moving_stays_stopped() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::under(d.path());
    let moving = HubState::Moving {
        id: ID.into(),
        hub: NEW_HUB.into(),
        address: Some(LOCAL),
    };
    set_state(&p, moving.clone());
    let app = app_at(&p, free_port().await);
    assert_eq!(app.hub_state.get(), moving);
    assert!(
        app.refresher.install_login(login("x")).await.is_err(),
        "gate stays closed after a restart"
    );
}

#[tokio::test]
async fn restart_after_abort_returns_to_active() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::under(d.path());
    set_state(&p, HubState::Aborted { id: ID.into() });
    let app = app_at(&p, free_port().await);
    assert_eq!(app.hub_state.get(), HubState::Active);
    assert!(app.refresher.install_login(login("x")).await.is_ok());
}

#[tokio::test]
async fn restart_after_commit_finishes_retiring_and_becomes_a_node() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::under(d.path());
    conflux::fsutil::ensure_private_dir(&p.accounts_dir()).unwrap();
    std::fs::write(p.accounts_dir().join("marker"), "x").unwrap();
    set_state(
        &p,
        HubState::Retired {
            id: ID.into(),
            hub: NEW_HUB.into(),
            address: Some(LOCAL),
        },
    );
    let port = free_port().await;
    let app = app_at(&p, port);
    assert!(hubmove::retired_dir(&p, ID).join("marker").exists());
    assert!(app.store.all().is_empty());
    assert_eq!(app.redirect_hub().as_deref(), Some(NEW_HUB));

    serve_new_hub(port).await;
    hubmove::continue_move(&app);
    eventually(|| app.lifecycle.is_draining().then_some(())).await;
    let cfg = Config::load(&p).unwrap();
    assert_eq!(cfg.role, Role::Node);
    assert_eq!(cfg.hub.as_deref(), Some(NEW_HUB));
    assert!(!hubmove::hub_state_file(&p).exists());
}

async fn node_with_an_unfinished_move(h: &Harness, bundle: &Bundle) -> (tempfile::TempDir, Paths, Arc<Node>) {
    let dir = tempfile::tempdir().unwrap();
    let p = Paths::under(dir.path());
    Config {
        role: Role::Node,
        hub: Some("127.0.0.1".into()),
        ..Config::default()
    }
    .save(&p)
    .unwrap();
    hubmove::import_bundle(&p, bundle, "127.0.0.1", h.port).unwrap();
    let node = Node::named(
        "node-one".into(),
        "127.0.0.1".into(),
        h.port,
        0,
        p.clone(),
        ClientPaths {
            claude_settings: dir.path().join("claude/settings.json"),
            codex_config: dir.path().join("codex/config.toml"),
            grok_config: dir.path().join("grok/config.toml"),
        },
        hub_run_by(1),
    );
    (dir, p, node)
}

#[tokio::test]
async fn a_node_restarted_mid_move_commits_again() {
    let h = hub().await;
    h.add(Provider::Claude, "a").await;
    let (_ws, bundle) = moving(&h).await;

    let (_dir, p, node) = node_with_an_unfinished_move(&h, &bundle).await;
    assert!(node.resume_move().await.unwrap());
    assert_eq!(Config::load(&p).unwrap().role, Role::Hub);
    assert!(p.has_accounts());
    assert!(hubmove::find_pending(&p).unwrap().is_none());
    assert!(node.lifecycle.is_draining());

    let fresh = hub().await;
    let (_dir, p, node) = node_with_an_unfinished_move(&fresh, &bundle).await;
    assert!(!node.resume_move().await.unwrap());
    assert_eq!(Config::load(&p).unwrap().role, Role::Node);
    assert!(!p.has_accounts());
    assert!(
        hubmove::find_pending(&p).unwrap().is_none(),
        "a cancelled move is discarded"
    );
    assert!(!node.lifecycle.is_draining());
}

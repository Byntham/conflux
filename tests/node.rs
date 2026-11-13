mod common;

use std::sync::Arc;
use std::time::Duration;

use common::Harness;
use conflux::accounts::Provider;
use conflux::config::{Config, Paths, Role};
use conflux::hubstate::HubState;
use conflux::node::Node;
use conflux::setup::{self, ClientPaths};
use conflux::update::UpdateStatus;
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct NodeHarness {
    node: Arc<Node>,
    base: String,
    port: u16,
    dir: tempfile::TempDir,
    clients: ClientPaths,
    http: reqwest::Client,
}

async fn start_node(hub_port: u16) -> NodeHarness {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let clients = ClientPaths {
        claude_settings: dir.path().join("claude/settings.json"),
        codex_config: dir.path().join("codex/config.toml"),
    };
    let node = Node::named(
        "node-one".into(),
        "127.0.0.1".into(),
        hub_port,
        port,
        Paths::under(dir.path()),
        clients.clone(),
    );
    let routes = node.routes();
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    NodeHarness {
        node,
        base: format!("http://127.0.0.1:{port}"),
        port,
        dir,
        clients,
        http: reqwest::Client::new(),
    }
}

fn claude(n: &NodeHarness) -> reqwest::RequestBuilder {
    n.http
        .post(format!("{}/v1/messages", n.base))
        .header("user-agent", "claude-cli/2.1.280 (external, cli)")
        .header("x-app", "cli")
        .header("authorization", "Bearer conflux")
        .json(&json!({"model": "claude-sonnet-5-5"}))
}

#[tokio::test]
async fn request_flows_through_node_and_hub() {
    let hub = Harness::new().await;
    hub.add(Provider::Claude, "a").await;
    let n = start_node(hub.port).await;
    let r = claude(&n).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["token"], "at-a");
    let upstream = hub.fake.http_headers.lock().unwrap()[0].clone();
    assert!(
        upstream.get("x-conflux-node").is_none(),
        "node header stays inside conflux"
    );
}

#[tokio::test]
async fn websocket_flows_through_node_and_hub() {
    let hub = Harness::new().await;
    hub.add(Provider::Codex, "c").await;
    let n = start_node(hub.port).await;
    let mut req = format!("ws://127.0.0.1:{}/v1/responses", n.port)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("user-agent", "codex_cli_rs/0.200.0".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(
        json!({"type": "response.create", "model": "m"}).to_string().into(),
    ))
    .await
    .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(v["type"], "response.created");
}

#[tokio::test]
async fn hub_down_names_the_hub_and_the_way_out() {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = probe.local_addr().unwrap().port();
    drop(probe);
    let n = start_node(dead_port).await;
    let r = claude(&n).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["type"], "error");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains("127.0.0.1") && message.contains("conflux off"));
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

async fn connect(hub: &Harness, n: &NodeHarness) -> tokio::task::JoinHandle<()> {
    let control = tokio::spawn(n.node.clone().run_control());
    eventually(|| hub.app.machines.list().into_iter().find(|m| m.online)).await;
    control
}

fn claude_base_url(clients: &ClientPaths) -> Option<String> {
    let text = std::fs::read_to_string(&clients.claude_settings).unwrap_or_default();
    let settings: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    settings["env"]["ANTHROPIC_BASE_URL"].as_str().map(str::to_string)
}

async fn set_routing(hub: &Harness, machine: &str, on: bool) -> reqwest::Response {
    hub.http
        .patch(format!("{}/conflux/api/machines/{machine}", hub.base))
        .json(&json!({ "routing": on }))
        .send()
        .await
        .unwrap()
}

fn fleet_routing(hub: &Harness) -> Vec<(String, Option<bool>)> {
    hub.app.fleet().iter().map(|m| (m.role.clone(), m.routing)).collect()
}

#[tokio::test]
async fn the_dashboard_turns_routing_on_and_off_for_each_machine() {
    let hub = Harness::new().await;
    let n = start_node(hub.port).await;
    let node_config = Paths::under(n.dir.path()).config_dir;
    setup::apply(&n.clients, &node_config, n.port).unwrap();
    setup::restore(&n.clients, &node_config, n.port).unwrap();
    setup::apply(&hub.app.clients, &Paths::under(hub.dir.path()).config_dir, hub.port).unwrap();
    let control = connect(&hub, &n).await;
    assert_eq!(hub.app.machines.list()[0].hostname, "node-one");
    assert_eq!(
        fleet_routing(&hub),
        [("hub".to_string(), Some(true)), ("node".to_string(), Some(false))]
    );

    let started = std::time::Instant::now();
    let r = set_routing(&hub, "node-one", true).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        claude_base_url(&n.clients),
        Some(format!("http://127.0.0.1:{}", n.port))
    );
    assert_eq!(
        fleet_routing(&hub),
        [("hub".to_string(), Some(true)), ("node".to_string(), Some(true))]
    );

    let r = set_routing(&hub, "hub-under-test", false).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(claude_base_url(&hub.app.clients), None);
    assert_eq!(
        claude_base_url(&n.clients),
        Some(format!("http://127.0.0.1:{}", n.port))
    );

    let r = set_routing(&hub, "node-one", false).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert_eq!(claude_base_url(&n.clients), None);
    assert_eq!(
        fleet_routing(&hub),
        [("hub".to_string(), Some(false)), ("node".to_string(), Some(false))]
    );

    let mut old = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/conflux/control", hub.port))
        .await
        .unwrap()
        .0;
    old.send(Message::Text(
        json!({ "hostname": "old-node", "version": "0.1.5" }).to_string().into(),
    ))
    .await
    .unwrap();
    eventually(|| hub.app.machines.list().into_iter().find(|m| m.hostname == "old-node")).await;
    assert_eq!(set_routing(&hub, "old-node", true).await.status(), StatusCode::CONFLICT);

    control.abort();
    eventually(|| {
        hub.app
            .machines
            .list()
            .into_iter()
            .find(|m| m.hostname == "node-one" && !m.online)
    })
    .await;
    assert_eq!(set_routing(&hub, "node-one", true).await.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn nodes_follow_a_hub_move_at_once() {
    let hub = Harness::new().await;
    let before = start_node(hub.port).await;
    connect(&hub, &before).await;
    let moving = HubState::Moving { id: "x".into() };
    assert!(hub.app.hub_state.compare_and_set(&HubState::Active, moving).unwrap());
    let r = hub
        .http
        .post(format!("{}/conflux/move/commit", hub.base))
        .json(&json!({"id": "x", "hub": "100.64.0.9"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    eventually(|| (before.node.hub() == "100.64.0.9").then_some(())).await;

    let after = start_node(hub.port).await;
    tokio::spawn(after.node.clone().run_control());
    eventually(|| (after.node.hub() == "100.64.0.9").then_some(())).await;
}

#[tokio::test]
async fn update_reaches_connected_nodes_and_shows_their_failures() {
    let hub = Harness::new().await;
    *hub.fake.release.lock().unwrap() = Some(("9.9.9".into(), "unused".into()));
    let n = start_node(hub.port).await;
    connect(&hub, &n).await;

    let v: Value = hub
        .http
        .post(format!("{}/conflux/api/update", hub.base))
        .json(&json!({"machines": ["node-one", "node-typo"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["version"], "9.9.9");
    assert_eq!(v["started"], json!(["node-one"]));
    assert_eq!(
        v["skipped"],
        json!([{"machine": "node-typo", "reason": "unknown machine"}])
    );

    let failed = eventually(|| {
        hub.app
            .fleet()
            .into_iter()
            .find(|m| matches!(m.update, Some(UpdateStatus::Failed { .. })))
    })
    .await;
    assert_eq!(failed.hostname, "node-one");
    assert!(failed.outdated && failed.online);
}

#[tokio::test]
async fn a_draining_node_closes_the_socket_only_after_the_turn_ends() {
    let hub = Harness::new().await;
    hub.add(Provider::Codex, "c").await;
    hub.ws_script("at-c", common::WsScript::SlowTurn);
    let n = start_node(hub.port).await;
    let mut req = format!("ws://127.0.0.1:{}/v1/responses", n.port)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("user-agent", "codex_cli_rs/0.200.0".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Text(
        json!({"type": "response.create", "model": "m"}).to_string().into(),
    ))
    .await
    .unwrap();
    let mut next_type = async || {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap();
        match frame {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<Value>(t.as_str()).unwrap()["type"]
                .as_str()
                .map(str::to_string),
            _ => None,
        }
    };
    assert_eq!(next_type().await.as_deref(), Some("response.created"));
    n.node.lifecycle.begin_drain();
    assert_eq!(
        next_type().await.as_deref(),
        Some("response.completed"),
        "the running turn finishes"
    );
    assert_eq!(next_type().await, None, "the idle socket closes");
}

#[tokio::test]
async fn only_tailnet_redirects_are_followed() {
    let hub = Harness::new().await;
    let n = start_node(hub.port).await;
    let paths = Paths::under(n.dir.path());
    Config {
        role: Role::Node,
        hub: Some("127.0.0.1".into()),
        ..Config::default()
    }
    .save(&paths)
    .unwrap();

    assert!(!n.node.follow_redirect("evil.example.com").await.unwrap());
    assert!(!n.node.follow_redirect("8.8.8.8").await.unwrap());
    assert_eq!(n.node.hub(), "127.0.0.1");

    assert!(n.node.follow_redirect("100.64.0.9").await.unwrap());
    assert_eq!(n.node.hub(), "100.64.0.9");
    assert_eq!(Config::load(&paths).unwrap().hub.as_deref(), Some("100.64.0.9"));
}

#[tokio::test]
async fn node_applies_its_own_host_and_origin_checks() {
    let hub = Harness::new().await;
    hub.add(Provider::Claude, "a").await;
    let n = start_node(hub.port).await;
    let r = claude(&n).header("host", "evil.example").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
    let r = claude(&n)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(hub.hits_for("at-a"), 0);
}

#[tokio::test]
async fn control_api_reaches_the_hub_through_the_node() {
    let hub = Harness::new().await;
    hub.add(Provider::Claude, "a").await;
    let n = start_node(hub.port).await;
    let v: Value = n
        .http
        .get(format!("{}/conflux/api/status", n.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["accounts"].as_array().unwrap().len(), 1);
    let v: Value = n
        .http
        .get(format!("{}/conflux/node", n.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["role"], "node");
    assert_eq!(v["hub"], "127.0.0.1");
}

#[test]
fn bare_conflux_prints_the_dashboard_link() {
    let dir = tempfile::tempdir().unwrap();
    let run = |role: Role, hub: Option<&str>| {
        Config {
            role,
            hub: hub.map(str::to_string),
            port: 4321,
            ..Config::default()
        }
        .save(&Paths::under(dir.path()))
        .unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_conflux"))
            .env("CONFLUX_HOME", dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };

    let node = run(Role::Node, Some("example-hub"));
    assert!(node.contains("Commands:"), "{node}");
    assert!(
        node.trim_end().ends_with("Dashboard: http://example-hub:4321"),
        "{node}"
    );

    let hub = run(Role::Hub, None);
    let last = hub.trim_end().lines().last().unwrap();
    assert!(
        last.starts_with("Dashboard: http://") && last.ends_with(":4321"),
        "{hub}"
    );

    assert!(!run(Role::Waiting, None).contains("Dashboard:"));

    let joined = std::process::Command::new(env!("CARGO_BIN_EXE_conflux"))
        .args(["join", "self", "--no-service"])
        .env("CONFLUX_HOME", dir.path())
        .env("HOME", dir.path())
        .env("CLAUDE_CONFIG_DIR", dir.path().join("claude"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .output()
        .unwrap();
    assert!(joined.status.success(), "{joined:?}");
    let joined = String::from_utf8(joined.stdout).unwrap();
    assert!(
        joined.contains("Dashboard: http://") && joined.contains(":4321"),
        "{joined}"
    );
}

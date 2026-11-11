mod common;

use std::sync::Arc;
use std::time::Duration;

use common::Harness;
use conflux::accounts::Provider;
use conflux::config::{Config, Paths, Role};
use conflux::hubstate::HubState;
use conflux::node::Node;
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
    http: reqwest::Client,
}

async fn start_node(hub_port: u16) -> NodeHarness {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let node = Node::named(
        "node-one".into(),
        "127.0.0.1".into(),
        hub_port,
        port,
        Paths::under(dir.path()),
    );
    let routes = node.routes();
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    NodeHarness {
        node,
        base: format!("http://127.0.0.1:{port}"),
        port,
        dir,
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

#[tokio::test]
async fn a_node_is_online_while_its_control_connection_is_open() {
    let hub = Harness::new().await;
    let n = start_node(hub.port).await;
    let control = connect(&hub, &n).await;
    let machines = hub.app.machines.list();
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].hostname, "node-one");
    control.abort();
    eventually(|| hub.app.machines.list().into_iter().find(|m| !m.online)).await;
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

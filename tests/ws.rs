mod common;

use std::time::Duration;

use common::{Harness, Reply, WsScript};
use conflux::accounts::Provider;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open(h: &Harness, extra: &[(&str, &str)]) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://127.0.0.1:{}/v1/responses", h.port)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("user-agent", "codex_cli_rs/0.200.0".parse().unwrap());
    for (k, v) in extra {
        req.headers_mut()
            .insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
}

async fn next_json(ws: &mut Client) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await.ok()?? {
            Ok(Message::Text(t)) => return serde_json::from_str(t.as_str()).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

fn create(model: &str) -> Message {
    Message::Text(
        json!({"type": "response.create", "model": model, "input": []})
            .to_string()
            .into(),
    )
}

#[tokio::test]
async fn relays_frames_with_account_auth_and_beta() {
    let h = Harness::new().await;
    let id = h.add(Provider::Codex, "c").await;
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("gpt-6.1-sol")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.created");
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "codex.rate_limits");
    let done = next_json(&mut ws).await.unwrap();
    assert_eq!(done["response"]["token"], "at-c");

    ws.send(create("gpt-6.1-sol")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["response"]["id"], "resp_at-c_2");

    let headers = h.fake.ws_headers.lock().unwrap()[0].clone();
    assert_eq!(
        headers.get("openai-beta").unwrap(),
        conflux::ws::RESPONSES_WEBSOCKETS_BETA
    );
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct-c");
    assert!(headers.get("origin").is_none());

    let snap = h.app.usage.get(&id);
    assert_eq!(snap.windows[0].used_pct, 33.0);
}

#[tokio::test]
async fn keeps_a_client_websocket_beta() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    let mut ws = open(&h, &[("openai-beta", "responses_websockets=2099-01-01")])
        .await
        .unwrap();
    ws.send(create("m")).await.unwrap();
    next_json(&mut ws).await.unwrap();
    let headers = h.fake.ws_headers.lock().unwrap()[0].clone();
    assert_eq!(headers.get("openai-beta").unwrap(), "responses_websockets=2099-01-01");
}

#[tokio::test]
async fn handshake_quota_rejection_fails_over() {
    let h = Harness::new().await;
    let a = h.add(Provider::Codex, "a").await;
    let b = h.add(Provider::Codex, "b").await;
    let (first, first_id, second) = if a < b {
        ("at-a", &a, "at-b")
    } else {
        ("at-b", &b, "at-a")
    };
    h.ws_script(first, WsScript::RejectHandshake429);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    next_json(&mut ws).await.unwrap();
    next_json(&mut ws).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["response"]["token"], second);
    assert_eq!(h.app.router.cooldowns(first_id).len(), 1);
}

#[tokio::test]
async fn handshake_401_with_a_failed_refresh_reports_the_401() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "a").await;
    h.ws_script("at-a", WsScript::RejectHandshake401);
    h.fake
        .refresh_replies
        .lock()
        .unwrap()
        .push_back(Reply::Json(503, vec![], "busy".into()));
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    let frame = next_json(&mut ws).await.unwrap();
    assert_eq!(frame["status"], 401);
    assert_eq!(frame["error"]["message"], "token revoked");
}

#[tokio::test]
async fn a_second_handshake_401_after_a_refresh_moves_to_the_next_account() {
    let h = Harness::new().await;
    let a = h.add(Provider::Codex, "a").await;
    let b = h.add(Provider::Codex, "b").await;
    let (first, first_id, second) = if a < b {
        ("at-a", &a, "at-b")
    } else {
        ("at-b", &b, "at-a")
    };
    h.ws_script(first, WsScript::RejectHandshake401);
    h.ws_script("renewed", WsScript::RejectHandshake401);
    h.fake.refresh_replies.lock().unwrap().push_back(Reply::Json(
        200,
        vec![],
        json!({"access_token": "renewed", "refresh_token": "next", "expires_in": 3600}).to_string(),
    ));
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    next_json(&mut ws).await.unwrap();
    next_json(&mut ws).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["response"]["token"], second);
    assert_eq!(h.app.router.cooldowns(first_id).len(), 1);
    let sent: Vec<String> = h
        .fake
        .ws_headers
        .lock()
        .unwrap()
        .iter()
        .map(|headers| headers["authorization"].to_str().unwrap().replace("Bearer ", ""))
        .collect();
    assert_eq!(sent, [first, "renewed", second]);
}

#[tokio::test]
async fn quota_error_mid_socket_is_forwarded_then_closed() {
    let h = Harness::new().await;
    let id = h.add(Provider::Codex, "c").await;
    h.ws_script("at-c", WsScript::QuotaMidStream);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    let err = next_json(&mut ws).await.unwrap();
    assert_eq!(err["error"]["type"], "usage_limit_reached");
    assert!(
        next_json(&mut ws).await.is_none(),
        "socket closes after the quota error"
    );
    assert_eq!(h.app.router.cooldowns(&id).len(), 1);
}

#[tokio::test]
async fn a_draining_hub_closes_the_socket_only_after_the_turn_ends() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    h.ws_script("at-c", WsScript::SlowTurn);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.created");
    h.app.lifecycle.begin_drain();
    assert_eq!(
        next_json(&mut ws).await.unwrap()["type"],
        "response.completed",
        "the running turn finishes"
    );
    assert!(next_json(&mut ws).await.is_none(), "the idle socket closes");
}

#[tokio::test]
async fn latency_stops_when_the_upstream_starts_answering() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    h.ws_script("at-c", WsScript::SlowTurn);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.created");
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.completed");
    let rows = h.app.requests.recent();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].transport, "ws");
    assert!(
        rows[0].latency_ms < 400,
        "latency {}ms includes the turn",
        rows[0].latency_ms
    );
}

#[tokio::test]
async fn no_accounts_sends_an_error_frame() {
    let h = Harness::new().await;
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    let err = next_json(&mut ws).await.unwrap();
    assert_eq!(err["type"], "error");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conflux login codex")
    );
}

#[tokio::test]
async fn browser_websocket_is_rejected() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    let r = open(&h, &[("origin", "https://evil.example")]).await;
    assert!(r.is_err());
    assert_eq!(h.hits_for("at-c"), 0);
}

#[tokio::test]
async fn continuation_goes_to_the_account_that_made_the_response() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "a").await;
    h.add(Provider::Codex, "b").await;
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    let created = next_json(&mut ws).await.unwrap();
    let resp_id = created["response"]["id"].as_str().unwrap().to_string();
    let owner = resp_id
        .trim_start_matches("resp_")
        .rsplit_once('_')
        .unwrap()
        .0
        .to_string();
    drop(ws);

    for _ in 0..3 {
        let mut ws = open(&h, &[]).await.unwrap();
        let msg = json!({"type": "response.create", "model": "m", "previous_response_id": resp_id});
        ws.send(Message::Text(msg.to_string().into())).await.unwrap();
        let created = next_json(&mut ws).await.unwrap();
        assert!(
            created["response"]["id"]
                .as_str()
                .unwrap()
                .starts_with(&format!("resp_{owner}_"))
        );
    }
}

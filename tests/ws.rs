mod common;

use std::time::Duration;

use common::{Harness, Reply, WsScript};
use conflux::accounts::Provider;
use conflux::requests::Speed;
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

    ws.send(create("gpt-6.2-sol")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["response"]["id"], "resp_at-c_2");
    while next_json(&mut ws).await.unwrap()["type"] != "response.completed" {}
    let nested = json!({"type": "response.create", "model": 5, "response": {"model": "gpt-6.2-astra"}});
    ws.send(Message::Binary(nested.to_string().into_bytes().into()))
        .await
        .unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["response"]["id"], "resp_at-c_3");
    let models: Vec<_> = h.app.requests.recent().into_iter().map(|r| r.model).collect();
    assert_eq!(
        models,
        [
            Some("gpt-6.2-astra".into()),
            Some("gpt-6.2-sol".into()),
            Some("gpt-6.1-sol".into())
        ]
    );

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
    async fn finish_turn(ws: &mut Client) {
        ws.send(create("m")).await.unwrap();
        while next_json(ws).await.unwrap()["type"] != "response.completed" {}
    }
    let h = Harness::new().await;
    let id = h.add(Provider::Codex, "c").await;
    h.ws_script("at-c", WsScript::Normal);
    h.ws_script("at-c", WsScript::QuotaMidStream);
    let mut idle = open(&h, &[]).await.unwrap();
    finish_turn(&mut idle).await;

    let throttle = json!({"error": {"type": "rate_limit_exceeded"}}).to_string();
    h.reply("at-c", Reply::Json(429, vec![("retry-after", "1".into())], throttle));
    let r = h
        .http
        .post(format!("{}/v1/responses", h.base))
        .header("user-agent", "codex_cli_rs/0.200.0")
        .header("content-encoding", "zstd")
        .body("compressed")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    finish_turn(&mut idle).await;

    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    let err = next_json(&mut ws).await.unwrap();
    assert_eq!(err["error"]["type"], "usage_limit_reached");
    assert!(
        next_json(&mut ws).await.is_none(),
        "socket closes after the quota error"
    );
    assert_eq!(h.app.router.cooldowns(&id).len(), 1);
    let closed = tokio::time::timeout(Duration::from_secs(5), idle.next()).await;
    assert!(
        matches!(closed, Ok(Some(Ok(Message::Close(_))) | None)),
        "an idle socket on the same account closes too"
    );
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
async fn the_row_is_logged_when_the_upstream_starts_answering() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    h.ws_script("at-c", WsScript::SlowTurn);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.created");
    let rows = h.app.requests.recent();
    assert_eq!(rows.len(), 1, "the row is logged before the turn ends");
    assert_eq!(rows[0].transport, "ws");
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.completed");
    assert_eq!(h.app.requests.recent()[0].usage.output, 7);

    h.ws_script("at-c", WsScript::FailedTurn);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.created");
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.failed");
    let usage = h.app.requests.recent()[0].usage;
    assert_eq!((usage.input, usage.cache_read, usage.output), (10, 40, 3));
    assert_eq!(usage.speed, Speed::Fast);
    let now = chrono::Utc::now();
    let kept: Vec<_> = h
        .app
        .requests
        .ledger()
        .read(now - chrono::Duration::hours(1), now)
        .into_iter()
        .map(|r| (r.usage.output, r.usage.speed))
        .collect();
    assert_eq!(kept, [(7, Speed::Standard), (3, Speed::Fast)]);

    h.ws_script("at-c", WsScript::CutTurn);
    let mut ws = open(&h, &[]).await.unwrap();
    ws.send(create("m")).await.unwrap();
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "response.created");
    assert!(next_json(&mut ws).await.is_none(), "the upstream hangs up mid-turn");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let now = chrono::Utc::now();
    let rows = h.app.requests.ledger().read(now - chrono::Duration::hours(1), now);
    assert_eq!(rows.len(), 2, "a turn cut off before it ends records nothing");
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

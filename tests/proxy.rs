mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use common::{Harness, Reply};
use conflux::accounts::{Provider, RefreshState};
use conflux::config::Paths;
use conflux::refresh::RefreshResult;
use http::StatusCode;
use serde_json::json;

fn quota_429_5h() -> Reply {
    let reset = (Utc::now() + chrono::Duration::hours(2)).timestamp().to_string();
    Reply::Json(
        429,
        vec![
            ("anthropic-ratelimit-unified-status", "rejected".into()),
            ("anthropic-ratelimit-unified-5h-status", "rejected".into()),
            ("anthropic-ratelimit-unified-5h-reset", reset),
        ],
        json!({"type": "error", "error": {"type": "rate_limit_error", "message": "limit"}}).to_string(),
    )
}

#[tokio::test]
async fn swaps_client_auth_for_account_token() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let r = h.claude(json!({"model": "claude-sonnet-5-5"})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token"], "at-a");
    assert_eq!(h.fake.hits.lock().unwrap()[0].0, "/v1/messages");
}

#[tokio::test]
async fn passes_compressed_bytes_through_unchanged() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    h.reply(
        "at-a",
        Reply::Raw(
            vec![
                ("content-encoding", "gzip".into()),
                ("content-type", "text/event-stream".into()),
            ],
            payload.clone(),
        ),
    );
    let client = reqwest::Client::builder().no_gzip().build().unwrap();
    let r = client
        .post(format!("{}/v1/messages", h.base))
        .header("user-agent", "claude-cli/2.1.280 (external, cli)")
        .header("x-app", "cli")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers().get("content-encoding").unwrap(), "gzip");
    assert_eq!(r.bytes().await.unwrap().to_vec(), payload);
}

#[tokio::test]
async fn quota_rejection_fails_over_and_cools_the_account() {
    let h = Harness::new().await;
    let a = h.add(Provider::Claude, "a").await;
    let b = h.add(Provider::Claude, "b").await;
    let first = if a < b { "at-a" } else { "at-b" };
    let first_id = if a < b { a.clone() } else { b.clone() };
    let second = if a < b { "at-b" } else { "at-a" };
    h.reply(first, quota_429_5h());
    let r = h.claude(json!({"model": "claude-sonnet-5-5"})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token"], second);
    assert_eq!(h.app.router.cooldowns(&first_id).len(), 1);

    let r = h.claude(json!({"model": "claude-sonnet-5-5"})).send().await.unwrap();
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token"], second, "cooled account is skipped");
}

#[tokio::test]
async fn all_accounts_limited_returns_last_upstream_429() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    h.reply("at-a", quota_429_5h());
    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), 429);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["message"], "limit");
    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), 429);
    assert!(r.headers().get("retry-after").is_some());
}

#[tokio::test]
async fn server_errors_are_not_replayed() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    h.add(Provider::Claude, "b").await;
    h.reply("at-a", Reply::Json(500, vec![], "{}".into()));
    h.reply("at-b", Reply::Json(500, vec![], "{}".into()));
    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), 500);
    assert_eq!(h.hits_for("at-a") + h.hits_for("at-b"), 1);
}

#[tokio::test]
async fn unauthorized_refreshes_then_retries_once() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    h.reply("at-a", Reply::Json(401, vec![], "{}".into()));
    h.fake.refresh_replies.lock().unwrap().push_back(Reply::Json(
        200,
        vec![],
        json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 28800}).to_string(),
    ));
    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token"], "at-new");
    let c = h.app.store.get(&id).unwrap().credentials();
    assert_eq!(c.generation, 2);
    assert_eq!(c.grant.refresh_token, "rt-new");
}

#[tokio::test]
async fn refresh_outcomes() {
    let h = Harness::new().await;
    let refresh = |r: Reply| h.fake.refresh_replies.lock().unwrap().push_back(r);

    let keep = h.add(Provider::Claude, "keep").await;
    refresh(Reply::Json(
        200,
        vec![],
        json!({"access_token": "x", "expires_in": 100}).to_string(),
    ));
    let e = h.app.store.get(&keep).unwrap();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Refreshed);
    assert_eq!(e.credentials().grant.refresh_token, "rt-keep");

    let codex = h.add(Provider::Codex, "cx").await;
    refresh(Reply::Json(
        200,
        vec![],
        json!({"access_token": "x", "expires_in": 100}).to_string(),
    ));
    let e = h.app.store.get(&codex).unwrap();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Unknown);
    let before = h.fake.hits.lock().unwrap().len();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Unknown);
    assert_eq!(
        h.fake.hits.lock().unwrap().len(),
        before,
        "an unknown grant is never posted again"
    );

    let bad = h.add(Provider::Claude, "bad").await;
    refresh(Reply::Json(400, vec![], json!({"error": "invalid_grant"}).to_string()));
    let e = h.app.store.get(&bad).unwrap();
    assert!(matches!(h.app.refresher.refresh(&e).await, RefreshResult::Rejected(_)));
    assert!(e.needs_login().is_some());

    let busy = h.add(Provider::Claude, "busy").await;
    refresh(Reply::Json(429, vec![("retry-after", "1".into())], "{}".into()));
    let e = h.app.store.get(&busy).unwrap();
    assert!(matches!(h.app.refresher.refresh(&e).await, RefreshResult::Waiting(_)));
    assert!(matches!(e.refresh_state(), RefreshState::RetryAt { retries: 1, .. }));

    let down = h.add(Provider::Claude, "down").await;
    refresh(Reply::Json(503, vec![], "busy".into()));
    let e = h.app.store.get(&down).unwrap();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Unknown);
}

#[tokio::test]
async fn login_replaces_an_unknown_refresh() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    h.fake
        .refresh_replies
        .lock()
        .unwrap()
        .push_back(Reply::Json(503, vec![], "".into()));
    let e = h.app.store.get(&id).unwrap();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Unknown);
    h.add(Provider::Claude, "a").await;
    assert_eq!(e.refresh_state(), RefreshState::Normal);
    assert_eq!(e.credentials().generation, 2);
    let reloaded = conflux::accounts::Store::load(&Paths::under(h.dir.path()).accounts_dir()).unwrap();
    assert_eq!(reloaded.get(&id).unwrap().refresh_state(), RefreshState::Normal);
}

#[tokio::test]
async fn browser_origins_are_rejected_on_proxy_routes() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let r = h
        .claude(json!({}))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.hits_for("at-a"), 0);
}

#[tokio::test]
async fn wrong_host_is_rejected() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let r = h
        .claude(json!({}))
        .header("host", "evil.example:8317")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn non_claude_code_clients_are_rejected() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let r = h
        .http
        .post(format!("{}/v1/messages", h.base))
        .header("user-agent", "curl/8")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.hits_for("at-a"), 0);
}

#[tokio::test]
async fn codex_paths_and_account_header() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    let r = h
        .http
        .post(format!("{}/v1/responses", h.base))
        .header("user-agent", "codex_cli_rs/0.200.0")
        .json(&json!({"model": "gpt-6.1-sol"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(h.fake.hits.lock().unwrap()[0].0, "/backend-api/codex/responses");
}

#[tokio::test]
async fn codex_usage_limit_fails_over() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c1").await;
    h.add(Provider::Codex, "c2").await;
    let body = json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 3600}}).to_string();
    h.reply("at-c1", Reply::Json(429, vec![], body.clone()));
    h.reply("at-c2", Reply::Json(429, vec![], body));
    h.reply("at-c1", Reply::Json(200, vec![], "{}".into()));
    let r = h
        .http
        .post(format!("{}/v1/responses", h.base))
        .header("user-agent", "codex_cli_rs/0.200.0")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(h.hits_for("at-c1"), 1);
    assert_eq!(h.hits_for("at-c2"), 1);
}

#[tokio::test]
async fn client_disconnect_cancels_upstream() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    h.reply("at-a", Reply::Endless);
    let mut r = h.claude(json!({})).send().await.unwrap();
    assert!(r.chunk().await.unwrap().is_some());
    drop(r);
    for _ in 0..40 {
        if h.fake.stream_dropped.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("upstream stream was not dropped after the client went away");
}

#[tokio::test]
async fn a_pinned_session_stays_on_its_account_with_many_requests_open() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    h.add(Provider::Claude, "b").await;
    let mut open = Vec::new();
    for _ in 0..12 {
        h.reply("at-a", Reply::Endless);
        h.reply("at-b", Reply::Endless);
        let r = h
            .claude(json!({}))
            .header("x-claude-code-session-id", "s1")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        open.push(r);
    }
    let mut hits = [h.hits_for("at-a"), h.hits_for("at-b")];
    hits.sort_unstable();
    assert_eq!(hits, [0, 12]);
}

#[tokio::test]
async fn no_accounts_message() {
    let h = Harness::new().await;
    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: serde_json::Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("conflux login claude"));
}

#[tokio::test]
async fn dashboard_shows_accounts_requests_and_tokens() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,",
        "\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":20,\"output_tokens\":1}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":44}}\n\n"
    );
    h.reply(
        "at-a",
        Reply::Raw(
            vec![("content-type", "text/event-stream".into())],
            sse.as_bytes().to_vec(),
        ),
    );
    let r = h
        .claude(json!({"model": "claude-sonnet-5-5"}))
        .header("x-conflux-node", "node-one")
        .send()
        .await
        .unwrap();
    assert_eq!(r.text().await.unwrap(), sse);

    let page = h
        .http
        .get(format!("{}/", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("<title>conflux</title>"));

    let mut record = serde_json::Value::Null;
    for _ in 0..40 {
        let v: serde_json::Value = h
            .http
            .get(format!("{}/conflux/api/dashboard", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["accounts"].as_array().unwrap().len(), 1);
        record = v["requests"][0].clone();
        if record["usage"]["output"] == 44 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(record["machine"], "node-one");
    assert_eq!(record["usage"]["cache_read"], 900);
    assert_eq!(record["usage"]["cache_write"], 20);
    assert_eq!(record["usage"]["output"], 44);
    assert!(h.fake.http_headers.lock().unwrap()[0].get("x-conflux-node").is_none());
}

#[tokio::test]
async fn requests_over_two_megabytes_pass_through() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let big = "x".repeat(5 * 1024 * 1024);
    let r = h.claude(json!({"model": "m", "blob": big})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(h.fake.body_lengths.lock().unwrap()[0] > 5 * 1024 * 1024);
}

#[tokio::test]
async fn requests_over_the_cap_are_refused() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let big = "x".repeat(33 * 1024 * 1024);
    let r = h.claude(json!({"model": "m", "blob": big})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(h.hits_for("at-a"), 0);
}

#[tokio::test]
async fn tailnet_names_are_allowed_once_tailscale_appears() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let host = format!("hub-box:{}", h.port);
    let r = h.claude(json!({})).header("host", &host).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
    h.app.apply_tailnet(conflux::tailnet::TailnetSelf {
        ips: vec!["100.101.102.103".parse().unwrap()],
        host_name: Some("hub-box".into()),
        dns_name: Some("hub-box.example.ts.net".into()),
    });
    let r = h.claude(json!({})).header("host", &host).send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn a_disconnecting_client_cannot_cancel_a_refresh() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    h.fake.refresh_replies.lock().unwrap().push_back(Reply::Delayed(
        Duration::from_millis(300),
        Box::new(Reply::Json(
            200,
            vec![],
            json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 28800}).to_string(),
        )),
    ));
    let e = h.app.store.get(&id).unwrap();
    let (refresher, account) = (h.app.refresher.clone(), e.clone());
    let caller = tokio::spawn(async move { refresher.refresh(&account).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    caller.abort();
    for _ in 0..40 {
        if e.credentials().generation == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(e.credentials().grant.refresh_token, "rt-new");
}

#[tokio::test]
async fn a_second_rejection_of_an_old_token_does_not_refresh_again() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    h.fake.refresh_replies.lock().unwrap().push_back(Reply::Json(
        200,
        vec![],
        json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 28800}).to_string(),
    ));
    let e = h.app.store.get(&id).unwrap();
    assert_eq!(h.app.refresher.refresh_rejected(&e, 1).await, RefreshResult::Refreshed);
    let hits = h.fake.hits.lock().unwrap().len();
    assert_eq!(h.app.refresher.refresh_rejected(&e, 1).await, RefreshResult::Refreshed);
    assert_eq!(h.fake.hits.lock().unwrap().len(), hits);
}

#[tokio::test]
async fn a_request_admitted_before_a_move_does_not_reach_the_upstream() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let moving = conflux::hubstate::HubState::Moving { id: "x".into() };
    assert!(
        h.app
            .hub_state
            .compare_and_set(&conflux::hubstate::HubState::Active, moving)
            .unwrap()
    );
    let req = http::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("user-agent", "claude-cli/2.1.280 (external, cli)")
        .header("x-app", "cli")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let r = conflux::proxy::handle(axum::extract::State(h.app.clone()), req).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.hits_for("at-a"), 0);
}

#[tokio::test]
async fn a_healthy_poll_clears_the_overage_flag() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    let mut headers = http::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-representative-claim",
        "overage".parse().unwrap(),
    );
    headers.insert("anthropic-ratelimit-unified-overage-status", "allowed".parse().unwrap());
    h.app.usage.observe_headers(&id, Provider::Claude, &headers);
    assert!(h.app.usage.get(&id).overage_in_use);
    h.reply(
        "at-a",
        Reply::Json(
            200,
            vec![],
            json!({"five_hour": {"utilization": 3.0}, "seven_day": {"utilization": 9.0}}).to_string(),
        ),
    );
    h.app.usage.poll(&h.app.store.get(&id).unwrap()).await.unwrap();
    assert!(!h.app.usage.get(&id).overage_in_use);
}

#[tokio::test]
async fn a_removed_account_is_not_refreshed_back_into_existence() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    let e = h.app.store.get(&id).unwrap();
    h.app.store.remove(&id).unwrap();
    let hits = h.fake.hits.lock().unwrap().len();
    assert!(matches!(
        h.app.refresher.refresh(&e).await,
        RefreshResult::LocalError(_)
    ));
    assert_eq!(h.fake.hits.lock().unwrap().len(), hits);
    assert!(
        !Paths::under(h.dir.path())
            .accounts_dir()
            .join(format!("{id}.json"))
            .exists()
    );
}

#[tokio::test]
async fn a_gzipped_codex_quota_error_still_cools_until_reset() {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(
        json!({"error": {"type": "usage_limit_reached", "resets_in_seconds": 3600}})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let mut headers = http::HeaderMap::new();
    headers.insert("content-encoding", "gzip".parse().unwrap());
    let rejection = conflux::proxy::classify_codex_429(&headers, &enc.finish().unwrap(), Some("m"), Utc::now());
    assert!(
        matches!(rejection, conflux::proxy::Rejection::Quota { .. }),
        "{rejection:?}"
    );
}

#[tokio::test]
async fn a_stale_entry_cannot_overwrite_a_new_login() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    let stale = h.app.store.get(&id).unwrap();
    h.app.store.remove(&id).unwrap();
    h.add(Provider::Claude, "a").await;
    let hits = h.fake.hits.lock().unwrap().len();
    assert!(matches!(
        h.app.refresher.refresh(&stale).await,
        RefreshResult::LocalError(_)
    ));
    assert_eq!(h.fake.hits.lock().unwrap().len(), hits);
}

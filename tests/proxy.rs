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
    quota_429_5h_resetting_in(chrono::Duration::hours(2))
}

fn quota_429_5h_resetting_in(wait: chrono::Duration) -> Reply {
    let reset = (Utc::now() + wait).timestamp().to_string();
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
    for reset_in in [chrono::Duration::hours(2), chrono::Duration::minutes(-1)] {
        let h = Harness::new().await;
        let a = h.add(Provider::Claude, "a").await;
        let b = h.add(Provider::Claude, "b").await;
        let first = if a < b { "at-a" } else { "at-b" };
        let first_id = if a < b { a.clone() } else { b.clone() };
        let second = if a < b { "at-b" } else { "at-a" };
        h.reply(first, quota_429_5h_resetting_in(reset_in));
        let r = h.claude(json!({"model": "claude-sonnet-5-5"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["token"], second);
        assert_eq!(h.app.router.cooldowns(&first_id).len(), 1);

        let r = h.claude(json!({"model": "claude-sonnet-5-5"})).send().await.unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(
            v["token"], second,
            "cooled account is skipped when the reset is {reset_in} away"
        );
    }
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

    let r = h
        .http
        .get(format!("{}/v1/models?client_version=0.200.0", h.base))
        .header("user-agent", "codex_cli_rs/0.200.0")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(h.fake.hits.lock().unwrap()[1].0, "/backend-api/codex/models");
    let logged = h.app.requests.recent();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].model.as_deref(), Some("gpt-6.1-sol"));
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

fn grok(h: &Harness, session: &str) -> reqwest::RequestBuilder {
    h.http
        .post(format!("{}/grok/v1/responses", h.base))
        .header("user-agent", "grok-shell/1.0.88 (linux; x86_64)")
        .header("authorization", "Bearer conflux")
        .header("x-grok-session-id", session)
        .json(&json!({"model": "grok-4.7", "stream": true}))
}

#[tokio::test]
async fn grok_requests_use_grok_accounts_and_fail_over_on_quota() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    let g1 = h.add(Provider::Grok, "g1").await;
    let g2 = h.add(Provider::Grok, "g2").await;
    let (first, first_id, second) = if g1 < g2 {
        ("at-g1", g1, "at-g2")
    } else {
        ("at-g2", g2, "at-g1")
    };
    let limit = json!({"error": {"code": "usage_pool_exhausted", "message": "limit", "resets_at": (Utc::now() + chrono::Duration::days(2)).timestamp()}});
    h.reply(first, Reply::Json(429, vec![], limit.to_string()));

    let r = grok(&h, "s1").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token"], second);
    let hits = h.fake.hits.lock().unwrap().clone();
    assert!(hits.iter().all(|(path, _)| path == "/grok-api/responses"), "{hits:?}");
    let cooled = h.app.router.cooldowns(&first_id);
    assert_eq!(cooled.len(), 1);
    assert!(cooled[0].until > Utc::now() + chrono::Duration::days(1));

    let r = grok(&h, "s2").send().await.unwrap();
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["token"], second);
    assert_eq!(h.hits_for("at-c"), 0);
}

#[tokio::test]
async fn grok_device_login_adds_the_account() {
    use base64::Engine;
    let h = Harness::new().await;
    let jwt = |claims: serde_json::Value| {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("x.{payload}.y")
    };
    let code = json!({"device_code": "dc", "user_code": "ABCD-EFGH", "verification_uri": "https://accounts.example/device", "interval": 1});
    h.reply("", Reply::Json(200, vec![], code.to_string()));
    let pending = json!({"error": "authorization_pending"}).to_string();
    let exp = (Utc::now() + chrono::Duration::hours(1)).timestamp();
    let tokens = json!({
        "access_token": jwt(json!({"sub": "user-1", "exp": exp})),
        "refresh_token": "rt-new",
        "id_token": jwt(json!({"email": "g@example.com"})),
    });
    {
        let mut refresh = h.fake.refresh_replies.lock().unwrap();
        refresh.push_back(Reply::Json(400, vec![], pending));
        refresh.push_back(Reply::Json(200, vec![], tokens.to_string()));
    }

    let start: serde_json::Value = h
        .http
        .post(format!("{}/conflux/api/login/grok/start", h.base))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(start["status"]["user_code"], "ABCD-EFGH");
    let id = start["login_id"].as_str().unwrap();
    let mut status = serde_json::Value::Null;
    for _ in 0..100 {
        status = h
            .http
            .get(format!("{}/conflux/api/login/{id}", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["state"] != "pending" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["email"], "g@example.com");
    let account = h.app.store.get(status["account"].as_str().unwrap()).unwrap();
    assert_eq!(account.provider, Provider::Grok);
    assert_eq!(account.credentials().grant.refresh_token, "rt-new");
    assert!((account.credentials().grant.expires_at.timestamp() - exp).abs() < 5);
}

#[tokio::test]
async fn dot_segments_cannot_leave_the_provider_path() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = Harness::new().await;
    h.add(Provider::Grok, "g").await;
    h.add(Provider::Codex, "c").await;
    for path in [
        "/grok/v1/../../oauth2/token",
        "/grok/v1/%2E%2e/x",
        "/grok/v1/..\\..\\x",
        "/grok/v1/%2e%2e\\billing",
        "/backend-api/codex/..\\..\\me",
        "/v1/messages\\..\\..\\x",
        "/backend-api/codex/..%2fme",
        "/grok/v1/..%5C..%5Cx",
    ] {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", h.port)).await.unwrap();
        let req = format!(
            "GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{}\r\nconnection: close\r\n\r\n",
            h.port
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 404"), "{path}: {out}");
    }
    assert_eq!(h.fake.hits.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn grok_usage_polls_and_refreshes_against_xai() {
    let h = Harness::new().await;
    let id = h.add(Provider::Grok, "g").await;
    let end = Utc::now() + chrono::Duration::days(3);
    let billing = json!({"config": {
        "currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY", "start": (end - chrono::Duration::days(7)).to_rfc3339(), "end": end.to_rfc3339()},
        "creditUsagePercent": 42.0
    }});
    h.reply("at-g", Reply::Json(200, vec![], billing.to_string()));
    let r = h
        .http
        .post(format!("{}/conflux/api/accounts/{id}/usage", h.base))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let snap = h.app.usage.get(&id);
    let w = snap.longest().unwrap();
    assert_eq!(w.used_pct, 42.0);
    h.reply("at-g", Reply::Json(200, vec![], json!({"config": {}}).to_string()));
    assert!(h.app.usage.poll(&h.app.store.get(&id).unwrap()).await.is_err());
    assert_eq!(
        h.app.usage.get(&id).longest().unwrap().used_pct,
        42.0,
        "a billing reading with no usage keeps the last known window"
    );

    let empty = h.add(Provider::Grok, "empty").await;
    let until = Utc::now() + chrono::Duration::hours(3);
    h.app
        .router
        .cool(&empty, conflux::router::ACCOUNT, until, "Grok usage limit");
    h.reply("at-empty", Reply::Json(200, vec![], json!({"config": {}}).to_string()));
    assert!(h.app.usage.poll(&h.app.store.get(&empty).unwrap()).await.is_err());
    assert_eq!(
        h.app.router.cooldowns(&empty).len(),
        1,
        "a billing reading with no credit window shows no room, so the cooldown stays"
    );
    assert_eq!(w.duration_secs, Some(7 * 86400));
    assert_eq!(w.resets_at.unwrap().timestamp(), end.timestamp());
    assert!(
        h.fake
            .hits
            .lock()
            .unwrap()
            .iter()
            .any(|(path, token)| path == "/grok-api/billing" && token == "at-g")
    );

    let throttled = h.add(Provider::Grok, "throttled").await;
    h.fake
        .refresh_replies
        .lock()
        .unwrap()
        .push_back(Reply::Json(429, vec![("retry-after", "1".into())], "{}".into()));
    let t = h.app.store.get(&throttled).unwrap();
    assert!(matches!(h.app.refresher.refresh(&t).await, RefreshResult::Waiting(_)));
    assert!(
        matches!(t.refresh_state(), RefreshState::RetryAt { .. }),
        "a throttled refresh tries again"
    );

    use base64::Engine;
    let exp = (Utc::now() + chrono::Duration::hours(6)).timestamp();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"sub": "g", "exp": exp}).to_string());
    h.fake.refresh_replies.lock().unwrap().push_back(Reply::Json(
        200,
        vec![],
        json!({"access_token": format!("x.{payload}.y"), "refresh_token": "rt-g2"}).to_string(),
    ));

    let e = h.app.store.get(&id).unwrap();
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Refreshed);
    assert_eq!(e.credentials().grant.refresh_token, "rt-g2");
    assert!(
        (e.credentials().grant.expires_at.timestamp() - exp).abs() < 5,
        "a refresh with no expires_in takes the token's own expiry"
    );
    assert!(
        h.fake
            .hits
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path == "/xai/oauth2/token")
    );
}

fn muse(h: &Harness, path: &str) -> reqwest::RequestBuilder {
    h.http
        .post(format!("{}{path}", h.base))
        .header("user-agent", "muse-build/1.4.23 (non-interactive; linux-x86_64)")
        .header("x-tbh-session-id", "s1")
        .json(&json!({"model": "muse-wonder-1.0", "stream": true}))
}

#[tokio::test]
async fn muse_requests_route_report_quota_and_fail_over() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    let m1 = h.add(Provider::Muse, "m1").await;
    let m2 = h.add(Provider::Muse, "m2").await;
    let (first, first_id, second, second_id) = if m1 < m2 {
        ("at-m1", m1, "at-m2", m2)
    } else {
        ("at-m2", m2, "at-m1", m1)
    };
    let resets = (Utc::now() + chrono::Duration::hours(3)).timestamp();
    let limit =
        json!({"error": {"code": "tier_limit", "type": "rate_limit_error", "message": "limit", "resets_at": resets}});
    h.reply(first, Reply::Json(429, vec![], limit.to_string()));
    let usage = json!({"type": "response.subscription_usage", "subscription": {
        "window": {"used_percent": 30, "resets_at": resets, "window_duration_mins": 300},
        "weekly": {"used_percent": 60, "resets_at": resets + 86400}
    }});
    h.reply(
        second,
        Reply::Raw(
            vec![("content-type", "text/event-stream".into())],
            format!("event: response.subscription_usage\ndata: {usage}\n\n").into_bytes(),
        ),
    );

    let r = muse(&h, "/muse/v1/responses").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let cooled = h.app.router.cooldowns(&first_id);
    assert_eq!(cooled.len(), 1);
    assert_eq!(cooled[0].until.timestamp(), resets);
    let snap = h.app.usage.get(&second_id);
    let weekly = snap.longest().unwrap();
    assert_eq!((weekly.key.as_str(), weekly.used_pct), ("weekly", 60.0));

    let r = h
        .http
        .get(format!("{}/muse-code/models", h.base))
        .header("x-tbh-session-id", "s1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    for path in ["key", "key/", "%6Bey", "/key", "Key"] {
        let r = h
            .http
            .post(format!("{}/muse-code/{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            404,
            "the pooled key never reaches the mint endpoint: {path}"
        );
    }
    let hits = h.fake.hits.lock().unwrap().clone();
    let paths: Vec<_> = hits.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    assert_eq!(
        paths,
        [
            ("/meta/v1/responses", first),
            ("/meta/v1/responses", second),
            ("/meta/muse-code/models", second)
        ]
    );
    for path in ["search", "browser_open", "feedback/v2"] {
        let r = h
            .http
            .post(format!("{}/muse-code/{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
    }
}

#[tokio::test]
async fn muse_device_login_mints_the_key_and_retries_a_failed_mint() {
    let h = Harness::new().await;
    let code = json!({"device_code": "dc", "user_code": "WXYZ-1234", "verification_uri": "https://auth.example/device", "interval": 1});
    h.reply("", Reply::Json(200, vec![], code.to_string()));
    h.reply(
        "",
        Reply::Json(400, vec![], json!({"error": "authorization_pending"}).to_string()),
    );
    h.reply(
        "",
        Reply::Json(200, vec![], json!({"access_token": "dca-oauth"}).to_string()),
    );
    h.reply("dca-oauth", Reply::Json(503, vec![], "busy".into()));
    let key = json!({"api_key": "LLM|1|k", "user_email": "M@example.com", "subs_tier_name": "Power"});
    h.reply("dca-oauth", Reply::Json(200, vec![], key.to_string()));

    let start: serde_json::Value = h
        .http
        .post(format!("{}/conflux/api/login/muse/start", h.base))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(start["status"]["user_code"], "WXYZ-1234");
    let id = start["login_id"].as_str().unwrap();
    let mut status = serde_json::Value::Null;
    for _ in 0..100 {
        status = h
            .http
            .get(format!("{}/conflux/api/login/{id}", h.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if status["state"] != "pending" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status["state"], "done", "{status}");
    let account = h.app.store.get(status["account"].as_str().unwrap()).unwrap();
    let c = account.credentials();
    assert_eq!(account.provider, Provider::Muse);
    assert_eq!(
        (c.grant.access_token.as_str(), c.grant.refresh_token.as_str()),
        ("LLM|1|k", "dca-oauth")
    );
    assert_eq!(
        (c.identity.as_str(), c.plan.as_deref()),
        ("m@example.com", Some("Power"))
    );
}

#[tokio::test]
async fn muse_polls_and_refreshes_by_minting_its_key() {
    let h = Harness::new().await;
    let id = h.add(Provider::Muse, "m").await;
    let key = |api_key: &str| {
        json!({"api_key": api_key, "subs_usage": {
            "window": {"used_percent": 12, "resets_at": 1900000000, "window_duration_mins": 300},
            "weekly": {"used_percent": 34, "resets_at": 1900500000}
        }})
        .to_string()
    };
    h.reply("rt-m", Reply::Json(200, vec![], key("at-m")));
    let e = h.app.store.get(&id).unwrap();
    h.app.usage.poll(&e).await.unwrap();
    assert_eq!(h.app.usage.get(&id).longest().unwrap().used_pct, 34.0);
    h.reply("rt-m", Reply::Json(200, vec![], json!({"api_key": "at-m"}).to_string()));
    assert!(h.app.usage.poll(&e).await.is_err());
    assert_eq!(h.app.usage.get(&id).longest().unwrap().used_pct, 34.0);

    h.reply("rt-m", Reply::Json(200, vec![], key("at-m-new")));
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Refreshed);
    let c = e.credentials();
    assert_eq!(
        (c.grant.access_token.as_str(), c.grant.refresh_token.as_str()),
        ("at-m-new", "rt-m")
    );

    h.reply("rt-m", Reply::Json(503, vec![], "busy".into()));
    assert!(matches!(h.app.refresher.refresh(&e).await, RefreshResult::Waiting(_)));
    assert!(e.needs_login().is_none());
    let slow = h.add(Provider::Muse, "slow").await;
    h.reply(
        "rt-slow",
        Reply::Json(429, vec![("retry-after", "3600".into())], "{}".into()),
    );
    let throttled = h.app.store.get(&slow).unwrap();
    match h.app.refresher.refresh(&throttled).await {
        RefreshResult::Waiting(at) => assert!(at > Utc::now() + chrono::Duration::minutes(59)),
        other => panic!("{other:?}"),
    }
    let mints = h.hits_for("rt-slow");
    assert!(h.app.usage.poll(&throttled).await.is_err());
    assert_eq!(
        h.hits_for("rt-slow"),
        mints,
        "the poll waits out the mint's Retry-After"
    );

    let fresh = h.add(Provider::Muse, "fresh").await;
    h.app
        .usage
        .observe_headers(&fresh, Provider::Muse, &http::HeaderMap::new());
    h.reply(
        "rt-fresh",
        Reply::Json(200, vec![], json!({"api_key": "at-fresh"}).to_string()),
    );
    assert!(h.app.usage.poll(&h.app.store.get(&fresh).unwrap()).await.is_err());
    assert!(
        h.app.usage.get(&fresh).updated_at.is_some(),
        "a mint with no usage still counts as a poll, so it is not repeated every minute"
    );

    let interrupted = e.credentials().generation + 1;
    e.journal.write_claim(interrupted).unwrap();
    e.journal
        .write_receipt(interrupted, &conflux::accounts::Receipt::Unknown)
        .unwrap();
    let reloaded = conflux::accounts::Store::load(&Paths::under(h.dir.path()).accounts_dir()).unwrap();
    let after_crash = reloaded.get(&id).unwrap();
    assert_eq!(after_crash.refresh_state(), RefreshState::Unknown);
    assert!(conflux::refresh::Refresher::due(
        &after_crash,
        Utc::now() + chrono::Duration::days(2)
    ));
    h.reply("rt-m", Reply::Json(200, vec![], key("at-m")));
    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::Refreshed);

    let gone = h.add(Provider::Muse, "gone").await;
    h.reply(
        "rt-gone",
        Reply::Json(401, vec![], json!({"title": "Authentication Error"}).to_string()),
    );
    let e = h.app.store.get(&gone).unwrap();
    assert!(matches!(h.app.refresher.refresh(&e).await, RefreshResult::Rejected(_)));
    assert!(
        h.fake
            .hits
            .lock()
            .unwrap()
            .iter()
            .all(|(path, _)| path == "/meta/muse-code/key")
    );
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
        ..Default::default()
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
    let moving = conflux::hubstate::HubState::Moving {
        id: "x".into(),
        hub: "node-one".into(),
        address: None,
    };
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
async fn usage_history_survives_a_hub_restart() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    h.reply(
        "at-a",
        Reply::Json(200, vec![], json!({"seven_day": {"utilization": 9.0}}).to_string()),
    );
    h.app.usage.poll(&h.app.store.get(&id).unwrap()).await.unwrap();
    let paths = Paths::under(h.dir.path());
    for _ in 0..100 {
        if paths.usage_history_file().exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let restarted = conflux::app::App::new(
        conflux::config::Config::default(),
        &paths,
        conflux::config::Endpoints::all_at("http://127.0.0.1:1"),
    )
    .unwrap();
    let history = restarted.usage.history();
    assert_eq!(
        history[&id][0].used["seven_day"], 9.0,
        "the restarted hub keeps the usage history: {history:?}"
    );
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

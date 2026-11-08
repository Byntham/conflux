mod common;

use common::{Harness, Reply};
use conflux::accounts::Provider;
use http::StatusCode;
use serde_json::{Value, json};

fn key(h: &Harness) -> String {
    format!("Bearer {}", h.app.management_key)
}

async fn post(h: &Harness, path: &str, body: Value) -> reqwest::Response {
    h.http
        .post(format!("{}/v0/management/{path}", h.base))
        .header("authorization", key(h))
        .json(&body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn rejects_a_missing_or_wrong_key() {
    let h = Harness::new().await;
    let r = h
        .http
        .get(format!("{}/v0/management/auth-files", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = h
        .http
        .get(format!("{}/v0/management/auth-files", h.base))
        .header("authorization", "Bearer nope")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_files_match_what_t3_decodes() {
    let h = Harness::new().await;
    let claude = h.add(Provider::Claude, "a").await;
    let codex = h.add(Provider::Codex, "c").await;
    h.app
        .store
        .get(&claude)
        .unwrap()
        .update_settings(|s| s.enabled = false)
        .unwrap();
    let v: Value = h
        .http
        .get(format!("{}/v0/management/auth-files", h.base))
        .header("authorization", key(&h))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    let c = files.iter().find(|f| f["id"] == codex.as_str()).unwrap();
    assert_eq!(c["auth_index"], codex.as_str());
    assert_eq!(c["provider"], "codex");
    assert_eq!(c["id_token"]["chatgpt_account_id"], "acct-c");
    assert!(
        c["id_token"].get("plan_type").is_none(),
        "T3 rejects nulls in optional fields"
    );
    let a = files.iter().find(|f| f["id"] == claude.as_str()).unwrap();
    assert_eq!(a["provider"], "claude");
    assert_eq!(a["disabled"], true);
    assert!(a["auth_index"].is_string());
}

#[tokio::test]
async fn api_call_substitutes_the_token_and_wraps_status() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    h.reply("at-a", Reply::Json(429, vec![], json!({"limited": true}).to_string()));
    let r = post(
        &h,
        "api-call",
        json!({
            "auth_index": id,
            "method": "GET",
            "url": "https://api.anthropic.com/api/oauth/usage",
            "header": {"Authorization": "Bearer $TOKEN$", "anthropic-beta": "oauth-2025-04-20"}
        }),
    )
    .await;
    assert_eq!(r.status(), 200, "upstream status travels inside the envelope");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status_code"], 429);
    assert!(v["body"].is_string());
    assert_eq!(
        serde_json::from_str::<Value>(v["body"].as_str().unwrap()).unwrap()["limited"],
        true
    );
    let hit = h.fake.hits.lock().unwrap().last().cloned().unwrap();
    assert_eq!(hit, ("/api/oauth/usage".to_string(), "at-a".to_string()));
}

#[tokio::test]
async fn api_call_refuses_other_urls_and_wrong_provider() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    let r = post(
        &h,
        "api-call",
        json!({"auth_index": id, "method": "GET", "url": "https://api.anthropic.com/v1/messages"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = post(
        &h,
        "api-call",
        json!({"auth_index": id, "method": "GET", "url": "https://chatgpt.com/backend-api/wham/usage"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(h.hits_for("at-a"), 0);
}

#[tokio::test]
async fn consume_sends_the_body_with_the_token_replaced() {
    let h = Harness::new().await;
    let id = h.add(Provider::Codex, "c").await;
    let r = post(
        &h,
        "api-call",
        json!({
            "auth_index": id,
            "method": "POST",
            "url": "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
            "header": {"Authorization": "Bearer $TOKEN$", "Content-Type": "application/json"},
            "data": "{\"credit_id\":\"x\"}"
        }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let hit = h.fake.hits.lock().unwrap().last().cloned().unwrap();
    assert_eq!(hit.0, "/backend-api/wham/rate-limit-reset-credits/consume");
}

#[tokio::test]
async fn reset_quota_clears_cooldowns() {
    let h = Harness::new().await;
    let id = h.add(Provider::Codex, "c").await;
    h.app.router.cool(
        &id,
        conflux::router::ACCOUNT,
        chrono::Utc::now() + chrono::Duration::hours(1),
        "x",
    );
    assert_eq!(h.app.router.cooldowns(&id).len(), 1);
    let r = post(&h, "reset-quota", json!({"auth_index": id})).await;
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["auth_index"], id.as_str());
    assert!(h.app.router.cooldowns(&id).is_empty());
    let r = post(&h, "reset-quota", json!({"auth_index": "missing"})).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

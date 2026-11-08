mod common;

use std::sync::Arc;
use std::time::Duration;

use common::Harness;
use conflux::accounts::{Provider, Receipt, RefreshState};
use conflux::app::App;
use conflux::config::{Config, Endpoints, Paths};
use conflux::hubmove::{self, Bundle};
use conflux::hubstate::{HubState, HubStateStore};
use conflux::refresh::RefreshResult;
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const ID: &str = "22222222-2222-4222-8222-222222222222";

async fn code(h: &Harness) -> String {
    let v: Value = post(h, "/conflux/api/hub/move-code", json!({}))
        .await
        .json()
        .await
        .unwrap();
    v["code"].as_str().unwrap().to_string()
}

async fn post(h: &Harness, path: &str, body: Value) -> reqwest::Response {
    h.http
        .post(format!("{}{path}", h.base))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn prepare(h: &Harness) -> Bundle {
    let code = code(h).await;
    let r = post(h, "/conflux/move/prepare", json!({"code": code, "id": ID})).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn commit(h: &Harness, id: &str) -> Value {
    post(h, "/conflux/move/commit", json!({"id": id, "hub": "node-one"}))
        .await
        .json()
        .await
        .unwrap()
}

fn paths(h: &Harness) -> Paths {
    Paths::under(h.dir.path())
}

#[tokio::test]
async fn wrong_codes_are_refused_and_burn_the_code() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    let good = code(&h).await;
    for _ in 0..5 {
        let r = post(&h, "/conflux/move/prepare", json!({"code": "WRONG", "id": ID})).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
    let r = post(&h, "/conflux/move/prepare", json!({"code": good, "id": ID})).await;
    assert_eq!(
        r.status(),
        StatusCode::FORBIDDEN,
        "too many failures invalidate the code"
    );
    assert_eq!(h.app.hub_state.get(), HubState::Active);
}

#[tokio::test]
async fn a_code_works_once() {
    let h = Harness::new().await;
    let good = code(&h).await;
    assert_eq!(
        post(&h, "/conflux/move/prepare", json!({"code": good, "id": ID}))
            .await
            .status(),
        200
    );
    post(&h, "/conflux/move/abort", json!({"id": ID})).await;
    let r = post(&h, "/conflux/move/prepare", json!({"code": good, "id": ID})).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn prepare_stops_service_and_exports_accounts_with_journals() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    let e = h.app.store.get(&id).unwrap();
    e.journal.write_claim(2).unwrap();
    e.journal.write_receipt(2, &Receipt::Unknown).unwrap();

    let bundle = prepare(&h).await;
    assert!(bundle.files.iter().any(|f| f.path == format!("{id}.json")));
    assert!(bundle.files.iter().any(|f| f.path.ends_with("receipt-2")));
    assert_eq!(bundle.management_key, h.app.management_key);
    assert_eq!(h.app.hub_state.get(), HubState::Moving { id: ID.into() });

    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("moving"));
    assert_eq!(h.hits_for("at-a"), 0);

    assert_eq!(h.app.refresher.refresh(&e).await, RefreshResult::GateClosed);
    assert!(h.app.refresher.install_login(login("b")).await.is_err());
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

#[tokio::test]
async fn commit_retires_the_store_and_is_idempotent() {
    let h = Harness::new().await;
    h.add(Provider::Claude, "a").await;
    prepare(&h).await;
    assert_eq!(commit(&h, ID).await["status"], "ok");
    let p = paths(&h);
    assert!(!p.accounts_dir().exists());
    assert!(hubmove::retired_dir(&p, ID).exists());
    assert_eq!(commit(&h, ID).await["status"], "ok", "a lost reply can be retried");

    let hb: Value = post(
        &h,
        "/conflux/heartbeat",
        json!({"hostname": "node-two", "version": "1"}),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(hb["hub"], "node-one");

    let r = h.claude(json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("node-one"));

    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = no_redirect.get(format!("{}/", h.base)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
    assert!(r.headers()["location"].to_str().unwrap().contains("node-one"));

    assert_eq!(
        post(&h, "/conflux/move/abort", json!({"id": ID})).await.status(),
        StatusCode::CONFLICT
    );
    assert!(matches!(h.app.hub_state.get(), HubState::Retired { .. }));
}

#[tokio::test]
async fn abort_returns_to_service() {
    let h = Harness::new().await;
    let id = h.add(Provider::Claude, "a").await;
    prepare(&h).await;
    assert_eq!(post(&h, "/conflux/move/abort", json!({"id": ID})).await.status(), 200);
    assert_eq!(h.app.hub_state.get(), HubState::Active);
    assert_eq!(h.claude(json!({})).send().await.unwrap().status(), 200);
    assert_eq!(
        commit(&h, ID).await["status"],
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
    let h = Harness::new().await;
    assert_eq!(
        post(&h, "/conflux/api/hub/resume", json!({})).await.status(),
        StatusCode::CONFLICT
    );
    prepare(&h).await;
    assert_eq!(post(&h, "/conflux/api/hub/resume", json!({})).await.status(), 200);
    assert_eq!(h.app.hub_state.get(), HubState::Active);
}

#[tokio::test]
async fn commit_with_another_id_is_refused() {
    let h = Harness::new().await;
    prepare(&h).await;
    assert_eq!(
        commit(&h, "33333333-3333-4333-8333-333333333333").await["status"],
        "aborted"
    );
    assert_eq!(h.app.hub_state.get(), HubState::Moving { id: ID.into() });
    assert!(paths(&h).accounts_dir().exists());
}

#[tokio::test]
async fn commit_and_resume_racing_never_leave_both() {
    for _ in 0..8 {
        let h = Arc::new(Harness::new().await);
        prepare(&h).await;
        let (a, b) = (h.clone(), h.clone());
        let commit_task = tokio::spawn(async move { commit(&a, ID).await });
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
async fn websockets_are_closed_when_a_move_starts() {
    let h = Harness::new().await;
    h.add(Provider::Codex, "c").await;
    let mut req = format!("ws://127.0.0.1:{}/v1/responses", h.port)
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
    for _ in 0..3 {
        ws.next().await.unwrap().unwrap();
    }
    prepare(&h).await;
    let next = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("socket closes promptly");
    assert!(matches!(next, None | Some(Ok(Message::Close(_))) | Some(Err(_))));
    assert_eq!(h.app.router.total_inflight(), 0);
}

fn app_at(p: &Paths) -> Arc<App> {
    App::new(Config::default(), p, Endpoints::all_at("http://127.0.0.1:1")).unwrap()
}

#[tokio::test]
async fn accounts_arrive_intact_on_the_new_hub() {
    let h = Harness::new().await;
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
    let bundle = prepare(&h).await;

    let new_dir = tempfile::tempdir().unwrap();
    let new_paths = Paths::under(new_dir.path());
    let (dir, pending) = hubmove::import_bundle(&new_paths, &bundle, "hub-box", 8317).unwrap();
    assert_eq!(commit(&h, ID).await["status"], "ok");
    hubmove::activate(&new_paths, &dir, &pending).unwrap();
    hubmove::discard_incoming(&dir).unwrap();

    let new_app = app_at(&new_paths);
    let moved = new_app.store.get(&id).unwrap();
    assert_eq!(moved.credentials().grant.refresh_token, "rt-a");
    assert!(matches!(
        moved.refresh_state(),
        RefreshState::RetryAt { retries: 1, .. }
    ));
    assert_eq!(new_app.management_key, h.app.management_key);
    assert_eq!(new_app.hub_state.get(), HubState::Active);
}

fn set_state(p: &Paths, state: HubState) {
    let s = HubStateStore::load(&hubmove::hub_state_file(p)).unwrap();
    assert!(s.compare_and_set(&HubState::Active, state).unwrap());
}

#[tokio::test]
async fn restart_while_moving_stays_stopped() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::under(d.path());
    set_state(&p, HubState::Moving { id: ID.into() });
    let app = app_at(&p);
    assert_eq!(app.hub_state.get(), HubState::Moving { id: ID.into() });
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
    let app = app_at(&p);
    assert_eq!(app.hub_state.get(), HubState::Active);
    assert!(app.refresher.install_login(login("x")).await.is_ok());
}

#[tokio::test]
async fn restart_finishes_an_interrupted_retirement() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::under(d.path());
    conflux::fsutil::ensure_private_dir(&p.accounts_dir()).unwrap();
    std::fs::write(p.accounts_dir().join("marker"), "x").unwrap();
    set_state(
        &p,
        HubState::Retired {
            id: ID.into(),
            hub: "node-one".into(),
        },
    );
    let app = app_at(&p);
    assert!(hubmove::retired_dir(&p, ID).join("marker").exists());
    assert!(app.store.all().is_empty());
    assert_eq!(app.redirect_hub().as_deref(), Some("node-one"));
}

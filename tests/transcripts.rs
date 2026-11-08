mod common;

use std::sync::Arc;

use common::Harness;
use conflux::config::{Config, Paths};
use conflux::hubstate::HubState;
use conflux::setup::ClientPaths;
use conflux::transcript_sync::TranscriptSync;
use http::StatusCode;
use serde_json::{Value, json};

const USER: &str =
    r#"{"type":"user","timestamp":"2026-11-07T10:00:00Z","cwd":"/srv/x","message":{"content":"Fix the flaky test"}}"#;
const FAILED: &str = r#"{"type":"user","timestamp":"2026-11-07T10:01:00Z","message":{"content":[{"type":"tool_result","is_error":true,"content":"linker cc not found"}]}}"#;
const DEBUG: &str = r#"{"type":"api-request-blob","message":{"content":"huge"}}"#;

struct Client {
    dir: tempfile::TempDir,
    sync: TranscriptSync,
}

impl Client {
    fn new(h: &Harness, machine: &str) -> Self {
        Self::pointing_at(h.base.clone(), machine)
    }

    fn pointing_at(base: String, machine: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clients = ClientPaths {
            claude_settings: dir.path().join("claude/settings.json"),
            codex_config: dir.path().join("codex/config.toml"),
        };
        let sync = TranscriptSync::new(
            machine.into(),
            Paths::under(dir.path()),
            &clients,
            Arc::new(move || base.clone()),
        );
        Self { dir, sync }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.path().join("claude/projects").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

async fn get(h: &Harness, path: &str) -> Value {
    h.http
        .get(format!("{}{path}", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn mirror(h: &Harness, machine: &str, rel: &str) -> String {
    let p = Paths::under(h.dir.path())
        .transcripts_dir()
        .unwrap()
        .join(machine)
        .join("claude")
        .join(rel);
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn sync_is_off_by_default() {
    assert!(!Config::default().transcript_sync);
    let parsed: Config = toml::from_str("port = 8317").unwrap();
    assert!(!parsed.transcript_sync);
}

#[tokio::test]
async fn pass_mirrors_new_lines_and_drops_debug_lines() {
    let h = Harness::new().await;
    let mut c = Client::new(&h, "node-one");
    c.write("p/a.jsonl", &format!("{USER}\n{DEBUG}\n"));
    c.sync.pass().await.unwrap();
    assert_eq!(mirror(&h, "node-one", "p/a.jsonl"), format!("{USER}\n"));

    c.sync.pass().await.unwrap();
    assert_eq!(
        mirror(&h, "node-one", "p/a.jsonl"),
        format!("{USER}\n"),
        "nothing new, nothing sent"
    );

    c.write("p/a.jsonl", &format!("{USER}\n{DEBUG}\n{FAILED}\n"));
    c.sync.pass().await.unwrap();
    assert_eq!(mirror(&h, "node-one", "p/a.jsonl"), format!("{USER}\n{FAILED}\n"));

    let v = get(&h, "/conflux/api/threads").await;
    let t = &v["threads"][0];
    assert_eq!(v["total"], 1);
    assert_eq!(t["machine"], "node-one");
    assert_eq!(t["title"], "Fix the flaky test");
    assert_eq!(t["turns"], 1);
    assert_eq!(t["errors"], 1);

    let id = t["id"].as_str().unwrap();
    let d = get(&h, &format!("/conflux/api/threads/{id}")).await;
    assert_eq!(d["events"].as_array().unwrap().len(), 2);
    assert_eq!(d["events"][1]["kind"], "failed");

    let s = get(&h, "/conflux/api/transcripts/search?q=linker").await;
    assert_eq!(s["hits"][0]["thread"], id);
}

#[tokio::test]
async fn offsets_survive_a_client_restart() {
    let h = Harness::new().await;
    let base = h.base.clone();
    let mut c = Client::new(&h, "node-two");
    c.write("a.jsonl", &format!("{USER}\n"));
    c.sync.pass().await.unwrap();

    let clients = ClientPaths {
        claude_settings: c.dir.path().join("claude/settings.json"),
        codex_config: c.dir.path().join("codex/config.toml"),
    };
    let mut restarted = TranscriptSync::new(
        "node-two".into(),
        Paths::under(c.dir.path()),
        &clients,
        Arc::new(move || base.clone()),
    );
    restarted.pass().await.unwrap();
    assert_eq!(mirror(&h, "node-two", "a.jsonl"), format!("{USER}\n"));
}

#[tokio::test]
async fn an_empty_new_hub_gets_everything_again() {
    let old = Harness::new().await;
    let new = Harness::new().await;
    let current = Arc::new(std::sync::Mutex::new(old.base.clone()));
    let dir = tempfile::tempdir().unwrap();
    let clients = ClientPaths {
        claude_settings: dir.path().join("claude/settings.json"),
        codex_config: dir.path().join("codex/config.toml"),
    };
    let reader = current.clone();
    let mut sync = TranscriptSync::new(
        "node-two".into(),
        Paths::under(dir.path()),
        &clients,
        Arc::new(move || reader.lock().unwrap().clone()),
    );
    let file = dir.path().join("claude/projects/a.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!("{USER}\n")).unwrap();
    sync.pass().await.unwrap();
    assert_eq!(mirror(&old, "node-two", "a.jsonl"), format!("{USER}\n"));

    *current.lock().unwrap() = new.base.clone();
    sync.pass().await.unwrap();
    assert_eq!(
        mirror(&new, "node-two", "a.jsonl"),
        format!("{USER}\n"),
        "unchanged files are sent too"
    );
}

#[tokio::test]
async fn a_large_batch_is_accepted() {
    let h = Harness::new().await;
    let mut c = Client::new(&h, "hub-box");
    let big = json!({"type": "user", "message": {"content": "z".repeat(3 * 1024 * 1024)}}).to_string();
    c.write("big.jsonl", &format!("{big}\n"));
    c.sync.pass().await.unwrap();
    assert_eq!(mirror(&h, "hub-box", "big.jsonl").len(), big.len() + 1);
}

#[tokio::test]
async fn unsafe_uploads_are_refused() {
    let h = Harness::new().await;
    for (machine, path) in [
        ("hub-box", "../../evil.jsonl"),
        (".state", "a.jsonl"),
        ("hub-box", "a.txt"),
    ] {
        let r = h
            .http
            .post(format!("{}/conflux/transcripts", h.base))
            .json(&json!({"machine": machine, "tool": "claude", "path": path, "offset": 0, "next_offset": 1, "data": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{machine} {path}");
    }
}

#[tokio::test]
async fn a_moving_hub_refuses_uploads_and_the_client_keeps_its_place() {
    let h = Harness::new().await;
    let mut c = Client::new(&h, "node-two");
    c.write("a.jsonl", &format!("{USER}\n"));
    let moving = HubState::Moving { id: "x".into() };
    assert!(
        h.app
            .hub_state
            .compare_and_set(&HubState::Active, moving.clone())
            .unwrap()
    );
    assert!(c.sync.pass().await.is_err());
    assert_eq!(mirror(&h, "node-two", "a.jsonl"), "");
    assert!(h.app.hub_state.compare_and_set(&moving, HubState::Active).unwrap());
    c.sync.pass().await.unwrap();
    assert_eq!(mirror(&h, "node-two", "a.jsonl"), format!("{USER}\n"));
}

#[tokio::test]
async fn thread_filters() {
    let h = Harness::new().await;
    let mut c = Client::new(&h, "hub-box");
    c.write("p/clean.jsonl", &format!("{USER}\n"));
    c.write("p/bad.jsonl", &format!("{USER}\n{FAILED}\n"));
    c.write("p/s/subagents/agent.jsonl", &format!("{USER}\n"));
    c.sync.pass().await.unwrap();
    assert_eq!(get(&h, "/conflux/api/threads").await["total"], 2);
    assert_eq!(get(&h, "/conflux/api/threads?subagents=true").await["total"], 3);
    assert_eq!(get(&h, "/conflux/api/threads?errors=true").await["total"], 1);
    assert_eq!(get(&h, "/conflux/api/threads?machine=node-two").await["total"], 0);
    let r = h
        .http
        .get(format!("{}/conflux/api/threads/nope", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

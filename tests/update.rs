mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{Harness, Reply};
use conflux::accounts::Provider;
use conflux::config::{Config, Paths, Role};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};

const BUILT: &str = env!("CARGO_BIN_EXE_conflux");

fn kill(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).output();
}

struct Started {
    child: Child,
    later_pids: Vec<u32>,
    _bin: tempfile::TempDir,
}

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in &self.later_pids {
            kill(*pid);
        }
    }
}

fn exe_name() -> String {
    format!("conflux{}", std::env::consts::EXE_SUFFIX)
}

#[cfg(unix)]
fn linked_like_the_path_entry(exe: &Path, bin: &Path) -> PathBuf {
    let link = bin.join(exe_name());
    std::os::unix::fs::symlink(exe, &link).unwrap();
    link
}

#[cfg(not(unix))]
fn linked_like_the_path_entry(exe: &Path, _bin: &Path) -> PathBuf {
    exe.to_path_buf()
}

fn start(command: &str, home: &Path, fake: &str, exe_dir: &str) -> Started {
    let bin = tempfile::tempdir().unwrap();
    let exe = bin.path().join(exe_dir).join(exe_name());
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::copy(BUILT, &exe).unwrap();
    let nothing_on_path = bin.path().join("empty");
    std::fs::create_dir(&nothing_on_path).unwrap();
    let launch = if exe_dir.is_empty() {
        exe
    } else {
        linked_like_the_path_entry(&exe, bin.path())
    };
    let mut conflux = Command::new(&launch);
    conflux
        .arg(command)
        .env("CONFLUX_HOME", home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", &nothing_on_path)
        .env("CONFLUX_ANTHROPIC_API", fake)
        .env("CONFLUX_CLAUDE_TOKEN", format!("{fake}/v1/oauth/token"))
        .env("CONFLUX_CLAUDE_AUTHORIZE", format!("{fake}/oauth/authorize"))
        .env("CONFLUX_CODEX_API", format!("{fake}/backend-api/codex"))
        .env("CONFLUX_CHATGPT_BACKEND", format!("{fake}/backend-api"))
        .env("CONFLUX_OPENAI_AUTH", fake)
        .env("CONFLUX_RELEASES", format!("{fake}/releases"))
        .stdin(Stdio::null());
    let child = (0..100)
        .find_map(|_| match conflux.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(50));
                None
            }
            spawned => Some(spawned.unwrap()),
        })
        .expect("the copied binary stayed busy");
    Started {
        child,
        later_pids: Vec::new(),
        _bin: bin,
    }
}

async fn eventually<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..600 {
        if let Some(found) = check().await {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn pid_at(url: &str) -> Option<u32> {
    let v: Value = reqwest::get(url).await.ok()?.json().await.ok()?;
    v["pid"].as_u64().map(|p| p as u32)
}

fn claude(http: &reqwest::Client, base: &str) -> reqwest::RequestBuilder {
    http.post(format!("{base}/v1/messages"))
        .header("user-agent", "claude-cli/2.1.280 (external, cli)")
        .header("x-app", "cli")
        .json(&json!({"model": "claude-sonnet-5-5"}))
}

fn codex(http: &reqwest::Client, base: &str, body: Value) -> reqwest::RequestBuilder {
    http.post(format!("{base}/v1/responses"))
        .header("user-agent", "codex_cli_rs/0.200.0")
        .json(&body)
}

fn message_hits(h: &Harness, token: &str) -> usize {
    let hits = h.fake.hits.lock().unwrap();
    hits.iter()
        .filter(|(path, t)| path == "/v1/messages" && t == token)
        .count()
}

fn offer_release(h: &Harness) {
    *h.fake.release.lock().unwrap() = Some(("999.0.0".into(), PathBuf::from(BUILT)));
}

const APP_HELPERS: &str = "Conflux.app/Contents/Helpers";

fn offer_app_release(h: &Harness, dir: &Path) {
    let path = dir.join("app.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file(format!("{APP_HELPERS}/{}", exe_name()), stored.unix_permissions(0o755))
        .unwrap();
    std::io::copy(&mut std::fs::File::open(BUILT).unwrap(), &mut zip).unwrap();
    zip.start_file("Conflux.app/Contents/Info.plist", stored).unwrap();
    std::io::Write::write_all(&mut zip, b"from the release").unwrap();
    zip.finish().unwrap();
    *h.fake.release.lock().unwrap() = Some(("999.0.0".into(), path));
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn a_hub_update_keeps_a_running_stream_and_its_routing_state() {
    let h = Harness::new().await;
    let a = h.add(Provider::Claude, "a").await;
    let b = h.add(Provider::Claude, "b").await;
    h.add(Provider::Codex, "c").await;
    h.add(Provider::Codex, "d").await;
    offer_release(&h);
    let port = free_port();
    Config {
        port,
        ..Config::default()
    }
    .save(&Paths::under(h.dir.path()))
    .unwrap();
    let mut hub = start("worker", h.dir.path(), &h.app.ep.anthropic_api, "");
    let base = format!("http://127.0.0.1:{port}");
    let status = format!("{base}/conflux/api/status");
    let old_pid = eventually("the hub to start", async || pid_at(&status).await).await;
    eventually("the first usage polls", async || {
        let hits = h.fake.hits.lock().unwrap();
        (hits.iter().filter(|(path, _)| path.ends_with("/usage")).count() >= 2).then_some(())
    })
    .await;

    let http = reqwest::Client::new();
    for token in ["at-a", "at-b"] {
        h.reply(
            token,
            Reply::Json(
                200,
                vec![("anthropic-ratelimit-unified-7d-utilization", "0.4".into())],
                json!({"token": token}).to_string(),
            ),
        );
    }
    let first: Value = claude(&http, &base).send().await.unwrap().json().await.unwrap();
    let preferred = first["token"].as_str().unwrap().to_string();
    let preferred_id = if preferred == "at-a" { &a } else { &b };
    let other = if preferred == "at-a" { "at-b" } else { "at-a" };
    h.reply(
        &preferred,
        Reply::Json(429, vec![("retry-after", "3600".into())], "{}".into()),
    );
    let failed_over: Value = claude(&http, &base).send().await.unwrap().json().await.unwrap();
    assert_eq!(failed_over["token"], other);
    let hits_on_cooling_account = message_hits(&h, &preferred);

    let dashboard = format!("{base}/conflux/api/dashboard");
    let away = async || {
        let d: Value = reqwest::get(&dashboard).await.ok()?.json().await.ok()?;
        d["machines"]
            .as_array()?
            .iter()
            .find(|m| m["hostname"] == "away")
            .cloned()
    };
    let (mut control, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/conflux/control"))
        .await
        .unwrap();
    let report = json!({"hostname": "away", "version": "0.0.1"}).to_string();
    control
        .send(tokio_tungstenite::tungstenite::Message::Text(report.into()))
        .await
        .unwrap();
    eventually("the node to appear", async || {
        away().await.filter(|m| m["online"] == true)
    })
    .await;
    drop(control);
    eventually("the node to go offline", async || {
        away().await.filter(|m| m["online"] == false)
    })
    .await;

    h.reply(other, Reply::Endless);
    let mut stream = claude(&http, &base).send().await.unwrap().bytes_stream();
    assert!(stream.next().await.unwrap().is_ok());

    let usual: Value = codex(&http, &base, json!({"model": "m"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let usual = usual["token"].as_str().unwrap().to_string();
    let owner = if usual == "at-c" { "at-d" } else { "at-c" };
    h.reply(&usual, Reply::Json(429, vec![("retry-after", "1".into())], "{}".into()));
    let event = |kind: &str| {
        let response = json!({"id": "resp_late", "usage": {"input_tokens": 0, "output_tokens": 77}});
        format!("data: {}\n\n", json!({"type": kind, "response": response}))
    };
    h.reply(
        owner,
        Reply::Delayed(
            Duration::from_secs(3),
            Box::new(Reply::EventsWithPause(
                event("response.created"),
                Duration::from_secs(1),
                event("response.completed"),
            )),
        ),
    );
    let late = tokio::spawn(codex(&http, &base, json!({"model": "m"})).send());
    eventually("the Codex request to reach its account", async || {
        let hits = h.fake.hits.lock().unwrap();
        hits.iter().any(|(_, token)| token == owner).then_some(())
    })
    .await;

    let asked: Value = http
        .post(format!("{base}/conflux/api/update"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(asked["started"].as_array().unwrap().len(), 1, "{asked}");
    let new_pid = eventually("the new hub to take over", async || {
        pid_at(&status).await.filter(|pid| *pid != old_pid)
    })
    .await;
    hub.later_pids.push(new_pid);

    for _ in 0..5 {
        assert!(
            stream.next().await.unwrap().is_ok(),
            "the stream that began before the update keeps flowing"
        );
    }
    let after: Value = claude(&http, &base).send().await.unwrap().json().await.unwrap();
    assert_eq!(after["token"], other);
    assert_eq!(
        message_hits(&h, &preferred),
        hits_on_cooling_account,
        "the new hub still knows which account is cooling down"
    );
    let late = late.await.unwrap().unwrap().text().await.unwrap();
    assert!(late.contains("response.completed"), "{late}");
    let next_turn = json!({"model": "m", "previous_response_id": "resp_late"});
    let continued: Value = codex(&http, &base, next_turn)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        continued["token"], owner,
        "the new hub knows which account owns a response that began on the old hub"
    );
    eventually(
        "the new hub to list the tokens of a request that ended on the old hub",
        async || {
            let d: Value = reqwest::get(&dashboard).await.ok()?.json().await.ok()?;
            let requests = d["requests"].as_array()?;
            requests.iter().any(|r| r["usage"]["output"] == 77).then_some(())
        },
    )
    .await;
    let d: Value = reqwest::get(&dashboard).await.unwrap().json().await.unwrap();
    assert_eq!(
        d["usage_history"][preferred_id][0]["used"]["seven_day"], 40.0,
        "the new hub keeps the usage history the old hub recorded: {}",
        d["usage_history"]
    );
    let still_listed = away().await.expect("the new hub still lists the offline node");
    assert_eq!(still_listed["online"], false);
    assert_eq!(still_listed["outdated"], true);
    assert!(
        hub.child.try_wait().unwrap().is_none(),
        "the old hub stays up while its stream is open"
    );

    drop(stream);
    eventually("the old hub to exit", async || {
        hub.child.try_wait().unwrap().map(|_| ())
    })
    .await;
    assert_eq!(pid_at(&status).await, Some(new_pid));
}

#[tokio::test]
async fn a_node_updates_on_the_hubs_command_and_its_supervisor_keeps_watch() {
    let Ok(hub_listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
        eprintln!("skipped: this machine has no IPv6 loopback to put a hub and a node on one port");
        return;
    };
    let port = hub_listener.local_addr().unwrap().port();
    let hub = Harness::on(hub_listener).await;
    hub.add(Provider::Claude, "a").await;
    let home = tempfile::tempdir().unwrap();
    let in_app = cfg!(unix);
    if in_app {
        offer_app_release(&hub, home.path());
    } else {
        offer_release(&hub);
    }
    Config {
        role: Role::Node,
        hub: Some("[::1]".into()),
        port,
        ..Config::default()
    }
    .save(&Paths::under(home.path()))
    .unwrap();
    let mut node = start(
        "serve",
        home.path(),
        &hub.app.ep.anthropic_api,
        if in_app { APP_HELPERS } else { "" },
    );
    let installed = node._bin.path().join("Conflux.app");
    let base = format!("http://127.0.0.1:{port}");
    let status = format!("{base}/conflux/node");
    let first_pid = eventually("the node to start", async || pid_at(&status).await).await;
    node.later_pids.push(first_pid);
    let name = eventually("the node to reach the hub", async || {
        hub.app
            .machines
            .list()
            .into_iter()
            .find(|m| m.online)
            .map(|m| m.hostname)
    })
    .await;

    let http = reqwest::Client::new();
    hub.reply(
        "at-a",
        Reply::Delayed(
            Duration::from_secs(3),
            Box::new(Reply::Json(200, vec![], json!({"slow": true}).to_string())),
        ),
    );
    let slow = tokio::spawn(claude(&http, &base).send());
    eventually("the slow request to reach the upstream", async || {
        (message_hits(&hub, "at-a") == 1).then_some(())
    })
    .await;

    let dashboard = format!("{base}/conflux/api/dashboard");
    let before: Value = http.get(&dashboard).send().await.unwrap().json().await.unwrap();
    assert_eq!(before["latest_version"], Value::Null, "{before}");
    let checked: Value = http
        .post(format!("{base}/conflux/api/update/check"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(checked["latest_version"], "999.0.0", "{checked}");
    let after: Value = http.get(&dashboard).send().await.unwrap().json().await.unwrap();
    let listed = after["machines"].as_array().unwrap();
    assert!(
        listed.iter().any(|m| m["hostname"] == name && m["outdated"] == true),
        "{after}"
    );

    let asked: Value = http
        .post(format!("{base}/conflux/api/update"))
        .json(&json!({"machines": [name]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(asked["started"], json!([name]), "{asked}");
    let second_pid = eventually("the new node to take over", async || {
        pid_at(&status).await.filter(|pid| *pid != first_pid)
    })
    .await;
    node.later_pids.push(second_pid);

    let answered: Value = slow.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(
        answered["slow"], true,
        "the request sent before the update still gets its answer"
    );
    assert_eq!(claude(&http, &base).send().await.unwrap().status(), 200);
    eventually("the new node to reach the hub", async || {
        hub.app.machines.list().into_iter().find(|m| m.online).map(|_| ())
    })
    .await;
    if in_app {
        assert_eq!(
            std::fs::read_to_string(installed.join("Contents/Info.plist")).unwrap(),
            "from the release"
        );
        eventually("the old app to be removed", async || {
            let leftover = std::fs::read_dir(node._bin.path())
                .unwrap()
                .any(|e| e.unwrap().file_name().to_string_lossy().starts_with("Conflux.app."));
            (!leftover).then_some(())
        })
        .await;
    }

    kill(second_pid);
    let third_pid = eventually("the supervisor to start a new worker", async || {
        pid_at(&status)
            .await
            .filter(|pid| *pid != first_pid && *pid != second_pid)
    })
    .await;
    node.later_pids.push(third_pid);

    let left = http
        .delete(format!("http://[::1]:{port}/conflux/api/machines/{name}"))
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(left.status(), 200, "{}", left.text().await.unwrap());
    let stopped = eventually("the supervisor to stop after the node leaves", async || {
        node.child.try_wait().unwrap()
    })
    .await;
    assert!(stopped.success(), "{stopped}");
    assert_eq!(pid_at(&status).await, None);
}

#[tokio::test]
async fn a_waiting_machine_updates_itself_and_keeps_waiting() {
    let releases = Harness::new().await;
    offer_release(&releases);
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    Config {
        role: Role::Waiting,
        port,
        ..Config::default()
    }
    .save(&Paths::under(home.path()))
    .unwrap();
    let mut waiting = start("serve", home.path(), &releases.app.ep.anthropic_api, "");
    let base = format!("http://127.0.0.1:{port}");
    let status = format!("{base}/conflux/api/status");
    let first_pid = eventually("the waiting machine to start", async || pid_at(&status).await).await;
    waiting.later_pids.push(first_pid);
    eventually("the waiting machine to see the new release", async || {
        let v: Value = reqwest::get(&status).await.ok()?.json().await.ok()?;
        (v["machines"][0]["outdated"] == true).then_some(())
    })
    .await;

    let asked: Value = reqwest::Client::new()
        .post(format!("{base}/conflux/api/update"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(asked["started"].as_array().map(Vec::len), Some(1), "{asked}");
    let second_pid = eventually("the new conflux to take over", async || {
        pid_at(&status).await.filter(|pid| *pid != first_pid)
    })
    .await;
    waiting.later_pids.push(second_pid);
    let now: Value = reqwest::get(&status).await.unwrap().json().await.unwrap();
    assert_eq!(now["role"], "waiting");
}

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Harness;
use conflux::config::{Config, Paths, Role};
use conflux::link::{self, Waiting};
use conflux::node::Node;
use conflux::setup::ClientPaths;
use conflux::tailnet::Tailscale;
use serde_json::{Value, json};

fn whois(user: u64, tags: &[&str]) -> Value {
    json!({ "Node": { "Name": "hub.example.ts.net.", "Tags": tags }, "UserProfile": { "ID": user } })
}

fn fake_tailscale(status: Value, caller: Arc<Mutex<Value>>) -> Tailscale {
    Arc::new(move |args: &[&str]| match args.first() {
        Some(&"status") => Some(status.clone()),
        Some(&"whois") => Some(caller.lock().unwrap().clone()),
        _ => None,
    })
}

struct Machine {
    paths: Paths,
    clients: ClientPaths,
    base: String,
    caller: Arc<Mutex<Value>>,
    running: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

async fn waiting_machine(listener: tokio::net::TcpListener) -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    assert_eq!(link::become_waiting(&paths).unwrap(), None);
    let clients = ClientPaths {
        claude_settings: dir.path().join("claude/settings.json"),
        codex_config: dir.path().join("codex/config.toml"),
    };
    let caller = Arc::new(Mutex::new(whois(1, &[])));
    let tailscale = fake_tailscale(json!({ "Self": { "UserID": 1 } }), caller.clone());
    let addr = listener.local_addr().unwrap();
    let waiting = Waiting::new(addr.port(), paths.clone(), clients.clone(), tailscale);
    let finishes_first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = tokio::spawn(waiting.run(vec![listener, finishes_first]));
    Machine {
        paths,
        clients,
        base: format!("http://{addr}"),
        caller,
        running,
        _dir: dir,
    }
}

fn peer(name: &str, os: &str, user: u64, ip: &str) -> Value {
    json!({ "HostName": name, "DNSName": format!("{name}.example.ts.net."), "OS": os, "UserID": user, "Online": true, "TailscaleIPs": [ip] })
}

async fn discover(hub: &Harness) -> Vec<Value> {
    let r = hub
        .http
        .get(format!("{}/conflux/api/discover", hub.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    v["machines"].as_array().unwrap().clone()
}

fn named<'a>(found: &'a [Value], name: &str) -> Option<&'a Value> {
    found.iter().find(|m| m["name"] == name)
}

#[tokio::test]
async fn the_hub_finds_a_waiting_machine_and_links_it_as_a_node() {
    let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = hub_listener.local_addr().unwrap().port();
    let status = json!({
        "Self": { "UserID": 1 },
        "Peer": {
            "a": peer("fresh", "linux", 1, "::1"),
            "b": peer("bare", "windows", 1, "192.0.2.1"),
            "c": peer("theirs", "linux", 2, "192.0.2.2"),
            "d": peer("phone", "iOS", 1, "192.0.2.3"),
        },
    });
    let hub = Harness::on_tailnet(hub_listener, fake_tailscale(status, Arc::default())).await;
    let machine = waiting_machine(tokio::net::TcpListener::bind(("::1", port)).await.unwrap()).await;

    let found = discover(&hub).await;
    assert_eq!(named(&found, "fresh").unwrap()["state"], "waiting");
    let bare = named(&found, "bare").unwrap();
    assert_eq!(bare["state"], "missing");
    assert!(bare["install"].as_str().unwrap().contains("install.ps1"));
    assert!(named(&found, "theirs").is_none());
    assert!(named(&found, "phone").is_none());

    let mut stalled = tokio::net::TcpStream::connect(("::1", port)).await.unwrap();
    let partial = format!("GET /conflux/link HTTP/1.1\r\nhost: [::1]:{port}\r\n");
    tokio::io::AsyncWriteExt::write_all(&mut stalled, partial.as_bytes())
        .await
        .unwrap();

    let r = hub
        .http
        .post(format!("{}/conflux/api/link", hub.base))
        .json(&json!({ "address": "::1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    tokio::time::timeout(Duration::from_secs(5), machine.running)
        .await
        .unwrap()
        .unwrap();
    let cfg = Config::load(&machine.paths).unwrap();
    assert_eq!(cfg.role, Role::Node);
    assert_eq!(cfg.hub.as_deref(), Some("[::1]"));
    let claude = std::fs::read_to_string(&machine.clients.claude_settings).unwrap();
    assert!(claude.contains(&format!("http://127.0.0.1:{port}")));

    let hub_on_tailnet = tokio::net::TcpListener::bind(("::1", port)).await.unwrap();
    let routes = hub.app.routes().into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(hub_on_tailnet, routes).await.unwrap() });

    let node_port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let node = Node::named("fresh".into(), cfg.hub.unwrap(), port, node_port, machine.paths.clone());
    tokio::spawn(node.run_control());
    for _ in 0..200 {
        if hub.app.fleet().iter().any(|m| m.hostname == "fresh" && m.online) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(hub.app.fleet().iter().any(|m| m.hostname == "fresh" && m.online));
    assert!(named(&discover(&hub).await, "fresh").is_none());
}

async fn ask_to_link(machine: &Machine) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{}/conflux/link", machine.base))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn a_waiting_machine_refuses_links_it_should_not_take() {
    let machine = waiting_machine(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()).await;

    *machine.caller.lock().unwrap() = whois(2, &[]);
    assert_eq!(ask_to_link(&machine).await, 403);
    *machine.caller.lock().unwrap() = whois(1, &["tag:server"]);
    assert_eq!(ask_to_link(&machine).await, 403);
    *machine.caller.lock().unwrap() = whois(1, &[]);

    let accounts = machine.paths.accounts_dir();
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::write(accounts.join("claude-a.json"), "{}").unwrap();
    assert_eq!(ask_to_link(&machine).await, 409);
    std::fs::remove_file(accounts.join("claude-a.json")).unwrap();

    let mut cfg = Config::load(&machine.paths).unwrap();
    cfg.role = Role::Node;
    cfg.hub = Some("other-hub".into());
    cfg.save(&machine.paths).unwrap();
    assert_eq!(ask_to_link(&machine).await, 409);

    assert_eq!(Config::load(&machine.paths).unwrap().hub.as_deref(), Some("other-hub"));
    assert!(!machine.clients.claude_settings.exists());
}

#[test]
fn only_a_machine_with_nothing_set_up_starts_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::create_dir_all(paths.accounts_dir()).unwrap();
    std::fs::write(paths.accounts_dir().join("claude-a.json"), "{}").unwrap();
    assert_eq!(link::become_waiting(&paths).unwrap(), Some(Role::Hub));
    assert!(!paths.config_file().exists());

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let clients = ClientPaths {
        claude_settings: dir.path().join("claude/settings.json"),
        codex_config: dir.path().join("codex/config.toml"),
    };
    Config {
        role: Role::Node,
        hub: Some("hub".into()),
        ..Config::default()
    }
    .save(&paths)
    .unwrap();
    conflux::setup::apply(&clients, &paths.config_dir, 8317).unwrap();
    assert_eq!(link::become_waiting(&paths).unwrap(), Some(Role::Node));
    assert_eq!(Config::load(&paths).unwrap().role, Role::Node);
}

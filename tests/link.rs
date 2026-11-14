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

fn fake_tailscale(status: Arc<Mutex<Value>>, caller: Arc<Mutex<Value>>) -> Tailscale {
    Arc::new(move |args: &[&str]| match args.first() {
        Some(&"status") => Some(status.lock().unwrap().clone()),
        Some(&"whois") => Some(caller.lock().unwrap().clone()),
        _ => None,
    })
}

struct Machine {
    paths: Paths,
    clients: ClientPaths,
    base: String,
    tailnet: Arc<Mutex<Value>>,
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
    let tailnet = Arc::new(Mutex::new(json!({ "Self": { "UserID": 1 } })));
    let tailscale = fake_tailscale(tailnet.clone(), caller.clone());
    let addr = listener.local_addr().unwrap();
    let waiting = Waiting::new(addr.port(), paths.clone(), clients.clone(), tailscale);
    let finishes_first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let running = tokio::spawn(waiting.run(vec![listener, finishes_first]));
    Machine {
        paths,
        clients,
        base: format!("http://{addr}"),
        tailnet,
        caller,
        running,
        _dir: dir,
    }
}

async fn connects_as_a_node(hub: &Harness, machine: &mut Machine, name: &str) {
    tokio::time::timeout(Duration::from_secs(5), &mut machine.running)
        .await
        .unwrap()
        .unwrap();
    let cfg = Config::load(&machine.paths).unwrap();
    assert_eq!(cfg.role, Role::Node);
    let hub_on_tailnet = tokio::net::TcpListener::bind(("::1", hub.port)).await.unwrap();
    let routes = hub.app.routes().into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(hub_on_tailnet, routes).await.unwrap() });
    let node_port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let node = Node::named(
        name.into(),
        cfg.hub.unwrap(),
        hub.port,
        node_port,
        machine.paths.clone(),
        machine.clients.clone(),
    );
    tokio::spawn(node.run_control());
}

fn post_in_background(hub: &Harness, action: &str, address: &str) -> tokio::task::JoinHandle<reqwest::Response> {
    let request = hub
        .http
        .post(format!("{}/conflux/api/{action}", hub.base))
        .json(&json!({ "address": address }));
    tokio::spawn(async move { request.send().await.unwrap() })
}

fn online(hub: &Harness, name: &str) -> bool {
    hub.app.fleet().iter().any(|m| m.hostname == name && m.online)
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
    let tailscale = fake_tailscale(Arc::new(Mutex::new(status)), Arc::default());
    let hub = Harness::on_tailnet(hub_listener, tailscale, "false").await;
    let mut machine = waiting_machine(tokio::net::TcpListener::bind(("::1", port)).await.unwrap()).await;

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

    let linking = post_in_background(&hub, "link", "::1");
    connects_as_a_node(&hub, &mut machine, "fresh").await;
    assert_eq!(Config::load(&machine.paths).unwrap().hub.as_deref(), Some("[::1]"));
    let claude = std::fs::read_to_string(&machine.clients.claude_settings).unwrap();
    assert!(claude.contains(&format!("http://127.0.0.1:{port}")));
    let r = linking.await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(online(&hub, "fresh"));
    assert!(named(&discover(&hub).await, "fresh").is_none());
}

#[cfg(unix)]
async fn install_on(hub: &Harness, address: &str) -> (reqwest::StatusCode, Value) {
    let r = hub
        .http
        .post(format!("{}/conflux/api/install", hub.base))
        .json(&json!({ "address": address }))
        .send()
        .await
        .unwrap();
    (r.status(), r.json().await.unwrap())
}

#[cfg(unix)]
#[tokio::test]
async fn the_hub_installs_conflux_over_ssh_and_links_the_machine_once_it_starts() {
    let tailscale = || {
        let status = json!({
            "Self": { "UserID": 1 },
            "Peer": { "a": peer("bare", "linux", 1, "::1"), "b": peer("pc", "windows", 1, "192.0.2.1") },
        });
        fake_tailscale(Arc::new(Mutex::new(status)), Arc::default())
    };
    let listener = || tokio::net::TcpListener::bind("127.0.0.1:0");

    let refused = Harness::on_tailnet(listener().await.unwrap(), tailscale(), "false").await;
    let (status, body) = install_on(&refused, "::1").await;
    assert_eq!(status, 502);
    assert!(body["error"].as_str().unwrap().contains("bare over ssh"), "{body}");
    let mut waits = waiting_machine(tokio::net::TcpListener::bind(("::1", refused.port)).await.unwrap()).await;
    let installing = post_in_background(&refused, "install", "::1");
    connects_as_a_node(&refused, &mut waits, "bare").await;
    assert_eq!(installing.await.unwrap().status(), 200);
    assert!(online(&refused, "bare"));

    let hub = Harness::on_tailnet(listener().await.unwrap(), tailscale(), "true").await;
    let found = discover(&hub).await;
    assert_eq!(named(&found, "bare").unwrap()["can_install"], true);
    assert_eq!(named(&found, "pc").unwrap()["can_install"], false);
    assert_eq!(install_on(&hub, "192.0.2.1").await.0, 400);
    assert_eq!(install_on(&hub, "192.0.2.2").await.0, 404);

    let installing = post_in_background(&hub, "install", "::1");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!installing.is_finished());
    assert_eq!(install_on(&hub, "::1").await.0, 409);
    let mut machine = waiting_machine(tokio::net::TcpListener::bind(("::1", hub.port)).await.unwrap()).await;
    connects_as_a_node(&hub, &mut machine, "bare").await;
    let r = installing.await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(online(&hub, "bare"));
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

async fn find_hubs(machine: &Machine) -> Value {
    let r = reqwest::get(format!("{}/conflux/api/hubs", machine.base))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json::<Value>().await.unwrap()["hubs"].clone()
}

async fn ask_to_join(machine: &Machine, address: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{}/conflux/api/join", machine.base))
        .json(&json!({ "address": address }))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn a_waiting_machine_finds_its_hub_and_links_itself_to_it() {
    let hub = Harness::new().await;
    let machine = waiting_machine(tokio::net::TcpListener::bind(("::1", hub.port)).await.unwrap()).await;
    let tailnet = |hub_owner: u64, hub_tags: &[&str]| {
        let mut hub = peer("localhost", "linux", hub_owner, "127.0.0.1");
        hub["Tags"] = json!(hub_tags);
        json!({ "Self": { "UserID": 1 }, "Peer": { "a": hub, "b": peer("fresh", "linux", 1, "::1") } })
    };

    *machine.tailnet.lock().unwrap() = tailnet(2, &[]);
    assert_eq!(find_hubs(&machine).await, json!([]));
    assert_eq!(ask_to_join(&machine, "127.0.0.1").await, 404);
    *machine.tailnet.lock().unwrap() = tailnet(1, &["tag:server"]);
    assert_eq!(find_hubs(&machine).await, json!([]));

    *machine.tailnet.lock().unwrap() = tailnet(1, &[]);
    assert_eq!(
        find_hubs(&machine).await,
        json!([{ "name": "localhost", "address": "127.0.0.1" }])
    );
    assert_eq!(ask_to_join(&machine, "::1").await, 502);
    assert_eq!(Config::load(&machine.paths).unwrap().role, Role::Waiting);

    assert_eq!(ask_to_join(&machine, "127.0.0.1").await, 200);
    tokio::time::timeout(Duration::from_secs(5), machine.running)
        .await
        .unwrap()
        .unwrap();
    let cfg = Config::load(&machine.paths).unwrap();
    assert_eq!(cfg.role, Role::Node);
    assert_eq!(cfg.hub.as_deref(), Some("127.0.0.1"));
    let claude = std::fs::read_to_string(&machine.clients.claude_settings).unwrap();
    assert!(claude.contains(&format!("http://127.0.0.1:{}", hub.port)));
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

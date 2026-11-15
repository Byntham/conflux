use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use crate::app::App;
use crate::guests::Access;
use crate::tailnet::{Caller, Identity};
use crate::update::UpdateStatus;

const SILENCE_LIMIT: Duration = Duration::from_secs(75);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub hostname: String,
    pub version: String,
    #[serde(default)]
    pub transcript_sync: bool,
    #[serde(default)]
    pub routing: Option<bool>,
    #[serde(default)]
    pub update: Value,
    #[serde(default)]
    pub ask: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Machine {
    pub hostname: String,
    pub version: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub online: bool,
    pub transcript_sync: bool,
    pub routing: Option<bool>,
    pub update: Option<UpdateStatus>,
    pub outdated: bool,
    pub guest: bool,
    pub tailnet_name: Option<String>,
    pub id: Option<String>,
    pub user: Option<String>,
    pub access: Option<Access>,
    #[serde(skip)]
    pub ask: String,
}

struct Link {
    id: u64,
    commands: mpsc::UnboundedSender<Value>,
    address: IpAddr,
}

struct Entry {
    report: Report,
    last_seen: DateTime<Utc>,
    node: Option<String>,
    guest: bool,
    tailnet_name: Option<String>,
    user: Option<String>,
    link: Option<Link>,
}

impl Entry {
    fn key(&self) -> String {
        self.node.clone().unwrap_or_else(|| self.report.hostname.clone())
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    report: Report,
    last_seen: DateTime<Utc>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    guest: bool,
    #[serde(default)]
    tailnet_name: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Default)]
pub struct Machines {
    map: Mutex<BTreeMap<String, Entry>>,
    next_link: AtomicU64,
    reported: Notify,
}

impl Machines {
    pub fn connect(
        &self,
        report: Report,
        address: IpAddr,
        peer: Option<&Identity>,
    ) -> (String, u64, mpsc::UnboundedReceiver<Value>) {
        let id = self.next_link.fetch_add(1, Ordering::SeqCst);
        let (commands, receiver) = mpsc::unbounded_channel();
        let entry = Entry {
            last_seen: Utc::now(),
            node: peer.map(|p| p.node.clone()),
            guest: peer.is_some_and(|p| !p.owner),
            tailnet_name: peer.and_then(|p| p.name.clone()),
            user: peer.and_then(|p| p.user.clone()),
            link: Some(Link { id, commands, address }),
            report,
        };
        let key = entry.key();
        let mut map = self.map.lock().unwrap();
        if !entry.guest {
            map.retain(|_, e| e.link.is_some() || e.guest || e.report.hostname != entry.report.hostname);
        }
        map.insert(key.clone(), entry);
        drop(map);
        self.reported.notify_waiters();
        (key, id, receiver)
    }

    pub fn report(&self, key: &str, link: u64, report: Report) {
        let mut map = self.map.lock().unwrap();
        if let Some(entry) = map.get_mut(key)
            && entry.link.as_ref().is_some_and(|l| l.id == link)
        {
            entry.report = Report {
                hostname: entry.report.hostname.clone(),
                ..report
            };
            entry.last_seen = Utc::now();
            self.reported.notify_waiters();
        }
    }

    pub async fn wait_for(&self, hostname: &str, limit: Duration, done: impl Fn(&Machine) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let reported = self.reported.notified();
            tokio::pin!(reported);
            reported.as_mut().enable();
            if self
                .list()
                .iter()
                .any(|m| m.hostname == hostname && !m.guest && done(m))
            {
                return true;
            }
            if tokio::time::timeout_at(deadline, reported).await.is_err() {
                return false;
            }
        }
    }

    pub fn disconnect(&self, key: &str, link: u64) {
        let mut map = self.map.lock().unwrap();
        if let Some(entry) = map.get_mut(key)
            && entry.link.as_ref().is_some_and(|l| l.id == link)
        {
            entry.link = None;
            entry.last_seen = Utc::now();
        }
    }

    pub fn send(&self, hostname: &str, command: Value) -> bool {
        let map = self.map.lock().unwrap();
        map.values()
            .filter(|e| !e.guest && e.report.hostname == hostname)
            .find_map(|e| e.link.as_ref())
            .is_some_and(|l| l.commands.send(command).is_ok())
    }

    pub fn broadcast(&self, command: &Value) {
        for entry in self.map.lock().unwrap().values() {
            if let Some(link) = &entry.link {
                let _ = link.commands.send(command.clone());
            }
        }
    }

    pub fn export(&self) -> Value {
        let now = Utc::now();
        let map = self.map.lock().unwrap();
        let known: Vec<_> = map
            .values()
            .map(|e| Saved {
                report: e.report.clone(),
                last_seen: if e.link.is_some() { now } else { e.last_seen },
                node: e.node.clone(),
                guest: e.guest,
                tailnet_name: e.tailnet_name.clone(),
                user: e.user.clone(),
            })
            .collect();
        json!(known)
    }

    pub fn import(&self, state: &Value) {
        let Ok(known) = serde_json::from_value::<Vec<Saved>>(state.clone()) else {
            return;
        };
        let mut map = self.map.lock().unwrap();
        for saved in known {
            let entry = Entry {
                report: Report {
                    update: Value::Null,
                    ..saved.report
                },
                last_seen: saved.last_seen,
                node: saved.node,
                guest: saved.guest,
                tailnet_name: saved.tailnet_name,
                user: saved.user,
                link: None,
            };
            map.insert(entry.key(), entry);
        }
    }

    pub fn addresses(&self) -> HashSet<IpAddr> {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter_map(|e| e.link.as_ref().map(|l| l.address))
            .collect()
    }

    pub fn list(&self) -> Vec<Machine> {
        let now = Utc::now();
        let mut machines: Vec<Machine> = self
            .map
            .lock()
            .unwrap()
            .values()
            .map(|e| Machine {
                hostname: e.report.hostname.clone(),
                version: e.report.version.clone(),
                role: "node".into(),
                last_seen: if e.link.is_some() { now } else { e.last_seen },
                online: e.link.is_some(),
                transcript_sync: e.report.transcript_sync,
                routing: e.report.routing,
                update: serde_json::from_value(e.report.update.clone()).unwrap_or(None),
                outdated: false,
                guest: e.guest,
                tailnet_name: e.tailnet_name.clone(),
                id: e.node.clone(),
                user: e.user.clone(),
                access: None,
                ask: e.report.ask.clone().unwrap_or_default(),
            })
            .collect();
        machines.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        machines
    }
}

pub fn hub_moved(hub: &str) -> Value {
    json!({ "type": "hub", "hub": hub })
}

pub fn access(state: Access, owner: Option<&str>) -> Value {
    json!({ "type": "access", "state": state, "owner": owner })
}

pub fn set_routing(on: bool) -> Value {
    json!({ "type": "routing", "on": on })
}

pub async fn control(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let peer = match app.identify(from.ip()).await {
        Caller::Unknown => {
            tracing::warn!(address = %from.ip(), "refused a machine that Tailscale could not identify");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "the hub could not ask Tailscale who this machine is",
            )
                .into_response();
        }
        Caller::Local => None,
        Caller::Peer(identity) => Some(identity),
    };
    upgrade.on_upgrade(move |socket| serve_control(app, socket, from.ip(), peer))
}

async fn next_report(node: &mut WebSocket) -> Option<Option<Report>> {
    match tokio::time::timeout(SILENCE_LIMIT, node.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => Some(serde_json::from_str(text.as_str()).ok()),
        Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => None,
        Ok(Some(Ok(_))) => Some(None),
    }
}

async fn serve_control(app: Arc<App>, mut node: WebSocket, address: IpAddr, peer: Option<Identity>) {
    let Some(Some(first)) = next_report(&mut node).await else {
        return;
    };
    let mut ask = first.ask.clone().unwrap_or_default();
    let (key, link, mut commands) = app.machines.connect(first, address, peer.as_ref());
    let guest = peer.filter(|p| !p.owner).map(|p| p.node);
    let mut access_changed = app.guests.watch();
    let mut told = None;
    if let Some(hub) = app.redirect_hub()
        && node
            .send(Message::Text(hub_moved(&hub).to_string().into()))
            .await
            .is_err()
    {
        app.machines.disconnect(&key, link);
        return;
    }
    let drain = app.lifecycle.drain_started();
    tokio::pin!(drain);
    loop {
        if let Some(guest) = &guest {
            let now = app.guests.access(guest, &ask);
            if told != Some(now) {
                let owner = app.tailnet.read().unwrap().owner.clone();
                let frame = access(now, owner.as_deref());
                if node.send(Message::Text(frame.to_string().into())).await.is_err() {
                    break;
                }
                if told == Some(Access::Approved) && now == Access::Denied {
                    break;
                }
                told = Some(now);
            }
        }
        tokio::select! {
            () = &mut drain => break,
            command = commands.recv() => {
                let Some(command) = command else { break };
                if node.send(Message::Text(command.to_string().into())).await.is_err() {
                    break;
                }
            }
            changed = access_changed.changed(), if guest.is_some() => {
                if changed.is_err() {
                    break;
                }
            }
            report = next_report(&mut node) => match report {
                Some(Some(report)) => {
                    ask = report.ask.clone().unwrap_or_default();
                    app.machines.report(&key, link, report);
                }
                Some(None) => {}
                None => break,
            },
        }
    }
    let _ = node.send(Message::Close(None)).await;
    app.machines.disconnect(&key, link);
}

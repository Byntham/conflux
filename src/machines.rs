use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use crate::app::App;
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
}

struct Link {
    id: u64,
    commands: mpsc::UnboundedSender<Value>,
    address: IpAddr,
}

struct Entry {
    report: Report,
    last_seen: DateTime<Utc>,
    link: Option<Link>,
}

#[derive(Default)]
pub struct Machines {
    map: Mutex<BTreeMap<String, Entry>>,
    next_link: AtomicU64,
    reported: Notify,
}

impl Machines {
    pub fn connect(&self, report: Report, address: IpAddr) -> (u64, mpsc::UnboundedReceiver<Value>) {
        let id = self.next_link.fetch_add(1, Ordering::SeqCst);
        let (commands, receiver) = mpsc::unbounded_channel();
        self.map.lock().unwrap().insert(
            report.hostname.clone(),
            Entry {
                report,
                last_seen: Utc::now(),
                link: Some(Link { id, commands, address }),
            },
        );
        self.reported.notify_waiters();
        (id, receiver)
    }

    pub fn report(&self, hostname: &str, link: u64, report: Report) {
        let mut map = self.map.lock().unwrap();
        if let Some(entry) = map.get_mut(hostname)
            && entry.link.as_ref().is_some_and(|l| l.id == link)
        {
            entry.report = Report {
                hostname: hostname.to_string(),
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
            if self.list().iter().any(|m| m.hostname == hostname && done(m)) {
                return true;
            }
            if tokio::time::timeout_at(deadline, reported).await.is_err() {
                return false;
            }
        }
    }

    pub fn disconnect(&self, hostname: &str, link: u64) {
        let mut map = self.map.lock().unwrap();
        if let Some(entry) = map.get_mut(hostname)
            && entry.link.as_ref().is_some_and(|l| l.id == link)
        {
            entry.link = None;
            entry.last_seen = Utc::now();
        }
    }

    pub fn send(&self, hostname: &str, command: Value) -> bool {
        let map = self.map.lock().unwrap();
        map.get(hostname)
            .and_then(|e| e.link.as_ref())
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
            .map(|e| (&e.report, if e.link.is_some() { now } else { e.last_seen }))
            .collect();
        json!(known)
    }

    pub fn import(&self, state: &Value) {
        let Ok(known) = serde_json::from_value::<Vec<(Report, DateTime<Utc>)>>(state.clone()) else {
            return;
        };
        let mut map = self.map.lock().unwrap();
        for (report, last_seen) in known {
            map.insert(
                report.hostname.clone(),
                Entry {
                    report: Report {
                        update: Value::Null,
                        ..report
                    },
                    last_seen,
                    link: None,
                },
            );
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
        self.map
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
            })
            .collect()
    }
}

pub fn hub_moved(hub: &str) -> Value {
    json!({ "type": "hub", "hub": hub })
}

pub fn set_routing(on: bool) -> Value {
    json!({ "type": "routing", "on": on })
}

pub async fn control(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade.on_upgrade(move |socket| serve_control(app, socket, from.ip()))
}

async fn next_report(node: &mut WebSocket) -> Option<Option<Report>> {
    match tokio::time::timeout(SILENCE_LIMIT, node.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => Some(serde_json::from_str(text.as_str()).ok()),
        Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => None,
        Ok(Some(Ok(_))) => Some(None),
    }
}

async fn serve_control(app: Arc<App>, mut node: WebSocket, address: IpAddr) {
    let Some(Some(first)) = next_report(&mut node).await else {
        return;
    };
    let hostname = first.hostname.clone();
    let (link, mut commands) = app.machines.connect(first, address);
    if let Some(hub) = app.redirect_hub() {
        app.machines.send(&hostname, hub_moved(&hub));
    }
    let drain = app.lifecycle.drain_started();
    tokio::pin!(drain);
    loop {
        tokio::select! {
            () = &mut drain => break,
            command = commands.recv() => {
                let Some(command) = command else { break };
                if node.send(Message::Text(command.to_string().into())).await.is_err() {
                    break;
                }
            }
            report = next_report(&mut node) => match report {
                Some(Some(report)) => app.machines.report(&hostname, link, report),
                Some(None) => {}
                None => break,
            },
        }
    }
    let _ = node.send(Message::Close(None)).await;
    app.machines.disconnect(&hostname, link);
}

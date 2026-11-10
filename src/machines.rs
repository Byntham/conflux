use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::app::App;
use crate::update::UpdateStatus;

pub const ONLINE_WINDOW: chrono::Duration = chrono::Duration::minutes(2);
const SILENCE_LIMIT: Duration = Duration::from_secs(75);

#[derive(Clone, Debug, Deserialize)]
pub struct Report {
    pub hostname: String,
    pub version: String,
    #[serde(default)]
    pub transcript_sync: bool,
    #[serde(default)]
    pub update: Option<UpdateStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Machine {
    pub hostname: String,
    pub version: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub online: bool,
    pub transcript_sync: bool,
    pub update: Option<UpdateStatus>,
    pub remote_update: bool,
    pub outdated: bool,
}

struct Link {
    id: u64,
    commands: mpsc::UnboundedSender<Value>,
}

struct Entry {
    report: Report,
    last_seen: DateTime<Utc>,
    has_control_channel: bool,
    link: Option<Link>,
}

#[derive(Default)]
pub struct Machines {
    map: Mutex<BTreeMap<String, Entry>>,
    next_link: AtomicU64,
}

impl Machines {
    pub fn heartbeat(&self, report: Report) {
        let mut map = self.map.lock().unwrap();
        if map.get(&report.hostname).is_some_and(|e| e.link.is_some()) {
            return;
        }
        map.insert(
            report.hostname.clone(),
            Entry {
                report,
                last_seen: Utc::now(),
                has_control_channel: false,
                link: None,
            },
        );
    }

    pub fn connect(&self, report: Report) -> (u64, mpsc::UnboundedReceiver<Value>) {
        let id = self.next_link.fetch_add(1, Ordering::SeqCst);
        let (commands, receiver) = mpsc::unbounded_channel();
        self.map.lock().unwrap().insert(
            report.hostname.clone(),
            Entry {
                report,
                last_seen: Utc::now(),
                has_control_channel: true,
                link: Some(Link { id, commands }),
            },
        );
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
                online: if e.has_control_channel {
                    e.link.is_some()
                } else {
                    now - e.last_seen < ONLINE_WINDOW
                },
                transcript_sync: e.report.transcript_sync,
                update: e.report.update.clone(),
                remote_update: e.has_control_channel,
                outdated: false,
            })
            .collect()
    }
}

pub async fn heartbeat(State(app): State<Arc<App>>, Json(report): Json<Report>) -> Json<Value> {
    app.machines.heartbeat(report);
    Json(json!({ "hub": app.redirect_hub() }))
}

pub fn hub_moved(hub: &str) -> Value {
    json!({ "type": "hub", "hub": hub })
}

pub async fn control(State(app): State<Arc<App>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| serve_control(app, socket))
}

async fn next_report(node: &mut WebSocket) -> Option<Option<Report>> {
    match tokio::time::timeout(SILENCE_LIMIT, node.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => Some(serde_json::from_str(text.as_str()).ok()),
        Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => None,
        Ok(Some(Ok(_))) => Some(None),
    }
}

async fn serve_control(app: Arc<App>, mut node: WebSocket) {
    let Some(Some(first)) = next_report(&mut node).await else {
        return;
    };
    let hostname = first.hostname.clone();
    let (link, mut commands) = app.machines.connect(first);
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

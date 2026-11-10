use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::App;

pub const ONLINE_WINDOW: chrono::Duration = chrono::Duration::minutes(2);

#[derive(Clone, Debug, Serialize)]
pub struct Machine {
    pub hostname: String,
    pub version: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    pub online: bool,
    pub transcript_sync: bool,
}

#[derive(Default)]
pub struct Machines {
    map: Mutex<BTreeMap<String, Machine>>,
}

impl Machines {
    pub fn seen(&self, hostname: &str, version: &str, role: &str, transcript_sync: bool) {
        self.map.lock().unwrap().insert(
            hostname.to_string(),
            Machine {
                hostname: hostname.into(),
                version: version.into(),
                role: role.into(),
                last_seen: Utc::now(),
                online: true,
                transcript_sync,
            },
        );
    }

    pub fn list(&self) -> Vec<Machine> {
        let now = Utc::now();
        self.map
            .lock()
            .unwrap()
            .values()
            .cloned()
            .map(|mut m| {
                m.online = now - m.last_seen < ONLINE_WINDOW;
                m
            })
            .collect()
    }
}

#[derive(Deserialize)]
pub struct Heartbeat {
    hostname: String,
    version: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    transcript_sync: bool,
}

pub async fn heartbeat(State(app): State<Arc<App>>, Json(hb): Json<Heartbeat>) -> Json<Value> {
    app.machines.seen(
        &hb.hostname,
        &hb.version,
        hb.role.as_deref().unwrap_or("node"),
        hb.transcript_sync,
    );
    Json(json!({ "hub": app.redirect_hub() }))
}

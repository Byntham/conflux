use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::Paths;
use crate::fsutil;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Guest {
    pub login: String,
    pub hostname: String,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub ask: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Pending,
    Approved,
    Denied,
}

#[derive(Clone)]
pub struct ApprovedGuest(pub String);

pub type GuestList = BTreeMap<String, Guest>;

pub struct Guests {
    path: PathBuf,
    list: Mutex<GuestList>,
    changed: watch::Sender<()>,
}

pub fn guests_file(paths: &Paths) -> PathBuf {
    paths.config_dir.join("guests.json")
}

impl Guests {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            list: Mutex::new(fsutil::read_json(path)?.unwrap_or_default()),
            changed: watch::channel(()).0,
        })
    }

    pub fn watch(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub fn export(&self) -> GuestList {
        self.list.lock().unwrap().clone()
    }

    pub fn is_approved(&self, id: &str) -> bool {
        self.list.lock().unwrap().get(id).is_some_and(|g| g.approved)
    }

    pub fn access(&self, id: &str, ask: &str) -> Access {
        match self.list.lock().unwrap().get(id) {
            Some(g) if g.approved => Access::Approved,
            Some(g) if g.ask == ask => Access::Denied,
            _ => Access::Pending,
        }
    }

    pub fn set(&self, id: &str, guest: Guest) -> Result<()> {
        let mut list = self.list.lock().unwrap();
        let mut next = list.clone();
        next.insert(id.to_string(), guest);
        if let Some(dir) = self.path.parent() {
            fsutil::ensure_private_dir(dir)?;
        }
        fsutil::write_json_durable(&self.path, &next)?;
        *list = next;
        self.changed.send_replace(());
        Ok(())
    }
}

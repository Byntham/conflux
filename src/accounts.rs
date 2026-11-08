use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fsutil;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Grant {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Credentials {
    pub id: String,
    pub provider: Provider,
    pub identity: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub chatgpt_account_id: Option<String>,
    pub grant: Grant,
    pub generation: u64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub allow_overage: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_overage: false,
        }
    }
}

pub fn account_id(provider: Provider, identity: &str) -> String {
    let digest = Sha256::digest(identity.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("{}-{}", provider.as_str(), hex)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum Receipt {
    Unknown,
    Ok { credentials: Box<Credentials> },
    Retry { next_at: DateTime<Utc>, retries: u32 },
    Rejected { reason: String },
}

pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(accounts_dir: &Path, id: &str) -> Self {
        Self {
            dir: accounts_dir.join(format!("{id}.journal")),
        }
    }

    fn claim_path(&self, n: u64) -> PathBuf {
        self.dir.join(format!("claim-{n}"))
    }
    fn receipt_path(&self, n: u64) -> PathBuf {
        self.dir.join(format!("receipt-{n}"))
    }

    pub fn has_claim(&self, n: u64) -> bool {
        self.claim_path(n).exists()
    }

    pub fn write_claim(&self, n: u64) -> Result<()> {
        fsutil::ensure_private_dir(&self.dir)?;
        fsutil::write_durable(&self.claim_path(n), Utc::now().to_rfc3339().as_bytes())
    }

    pub fn read_receipt(&self, n: u64) -> Result<Option<Receipt>> {
        fsutil::read_json(&self.receipt_path(n))
    }

    pub fn write_receipt(&self, n: u64, r: &Receipt) -> Result<()> {
        fsutil::ensure_private_dir(&self.dir)?;
        fsutil::write_json_durable(&self.receipt_path(n), r)
    }

    pub fn cleanup(&self, generation: u64) -> Result<()> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut removed = false;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let num = name
                .strip_prefix("claim-")
                .or_else(|| name.strip_prefix("receipt-"))
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(n) = num
                && n <= generation
            {
                std::fs::remove_file(entry.path())?;
                removed = true;
            }
        }
        if removed {
            fsutil::sync_dir(&self.dir)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RefreshState {
    Normal,
    RetryAt { at: DateTime<Utc>, retries: u32 },
    Unknown,
    Rejected(String),
}

pub struct AccountEntry {
    pub id: String,
    pub provider: Provider,
    creds: RwLock<Credentials>,
    settings: RwLock<Settings>,
    refresh: Mutex<RefreshState>,
    pub op_lock: tokio::sync::Mutex<()>,
    pub journal: Journal,
    accounts_dir: PathBuf,
}

impl AccountEntry {
    pub fn credentials(&self) -> Credentials {
        self.creds.read().unwrap().clone()
    }
    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }
    pub fn refresh_state(&self) -> RefreshState {
        self.refresh.lock().unwrap().clone()
    }
    pub fn set_refresh_state(&self, s: RefreshState) {
        *self.refresh.lock().unwrap() = s;
    }
    pub fn access_token(&self) -> String {
        self.creds.read().unwrap().grant.access_token.clone()
    }

    pub fn needs_login(&self) -> Option<String> {
        match self.refresh_state() {
            RefreshState::Rejected(r) => Some(format!("refresh rejected: {r}")),
            RefreshState::Unknown => {
                if self.credentials().grant.expires_at <= Utc::now() {
                    Some("access token expired after a refresh with an unknown outcome".into())
                } else {
                    None
                }
            }
            _ => {
                if self.credentials().grant.expires_at <= Utc::now() {
                    Some("access token expired".into())
                } else {
                    None
                }
            }
        }
    }

    fn creds_path(&self) -> PathBuf {
        self.accounts_dir.join(format!("{}.json", self.id))
    }
    fn settings_path(&self) -> PathBuf {
        self.accounts_dir.join(format!("{}.settings.json", self.id))
    }

    pub fn apply_ok(&self, creds: Credentials) -> Result<()> {
        fsutil::write_json_durable(&self.creds_path(), &creds)?;
        let generation = creds.generation;
        *self.creds.write().unwrap() = creds;
        self.set_refresh_state(RefreshState::Normal);
        self.journal.cleanup(generation)?;
        Ok(())
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) -> Result<Settings> {
        let mut s = self.settings.write().unwrap();
        let mut next = s.clone();
        f(&mut next);
        fsutil::write_json_durable(&self.settings_path(), &next)?;
        *s = next.clone();
        Ok(next)
    }
}

pub struct Store {
    dir: PathBuf,
    accounts: RwLock<BTreeMap<String, Arc<AccountEntry>>>,
}

impl Store {
    pub fn load(accounts_dir: &Path) -> Result<Self> {
        fsutil::ensure_private_dir(accounts_dir)?;
        let mut map = BTreeMap::new();
        for entry in std::fs::read_dir(accounts_dir)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.ends_with(".settings.json") || name.starts_with('.') {
                continue;
            }
            let creds: Credentials = fsutil::read_json(&path)?.context("account file vanished")?;
            let e = Self::make_entry(accounts_dir, creds)?;
            recover(&e)?;
            map.insert(e.id.clone(), Arc::new(e));
        }
        Ok(Self {
            dir: accounts_dir.to_path_buf(),
            accounts: RwLock::new(map),
        })
    }

    fn make_entry(dir: &Path, creds: Credentials) -> Result<AccountEntry> {
        let settings: Settings =
            fsutil::read_json(&dir.join(format!("{}.settings.json", creds.id)))?.unwrap_or_default();
        Ok(AccountEntry {
            id: creds.id.clone(),
            provider: creds.provider,
            journal: Journal::new(dir, &creds.id),
            creds: RwLock::new(creds),
            settings: RwLock::new(settings),
            refresh: Mutex::new(RefreshState::Normal),
            op_lock: tokio::sync::Mutex::new(()),
            accounts_dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn all(&self) -> Vec<Arc<AccountEntry>> {
        self.accounts.read().unwrap().values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<AccountEntry>> {
        self.accounts.read().unwrap().get(id).cloned()
    }

    pub fn entry_for_login(&self, provider: Provider, identity: &str) -> Result<Arc<AccountEntry>> {
        let id = account_id(provider, identity);
        if let Some(e) = self.get(&id) {
            return Ok(e);
        }
        let placeholder = Credentials {
            id: id.clone(),
            provider,
            identity: identity.to_string(),
            email: None,
            plan: None,
            chatgpt_account_id: None,
            grant: Grant {
                access_token: String::new(),
                refresh_token: String::new(),
                expires_at: DateTime::<Utc>::MIN_UTC,
                id_token: None,
            },
            generation: 0,
            updated_at: Utc::now(),
        };
        let e = Arc::new(Self::make_entry(&self.dir, placeholder)?);
        let mut map = self.accounts.write().unwrap();
        Ok(map.entry(id).or_insert(e).clone())
    }

    pub fn forget_if_unsaved(&self, id: &str) {
        let mut map = self.accounts.write().unwrap();
        if map.get(id).is_some_and(|e| e.credentials().generation == 0) {
            map.remove(id);
        }
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let removed = self.accounts.write().unwrap().remove(id);
        if removed.is_none() {
            return Ok(false);
        }
        fsutil::remove_durable(&self.dir.join(format!("{id}.json")))?;
        fsutil::remove_durable(&self.dir.join(format!("{id}.settings.json")))?;
        let jdir = self.dir.join(format!("{id}.journal"));
        if jdir.exists() {
            std::fs::remove_dir_all(&jdir)?;
            fsutil::sync_dir(&self.dir)?;
        }
        Ok(true)
    }
}

fn recover(e: &AccountEntry) -> Result<()> {
    let n = e.credentials().generation + 1;
    match e.journal.read_receipt(n)? {
        Some(Receipt::Ok { credentials }) => {
            tracing::info!(account = %e.id, generation = n, "recovering a finished refresh from its receipt");
            e.apply_ok(*credentials)?;
        }
        Some(Receipt::Retry { next_at, retries }) => {
            e.set_refresh_state(RefreshState::RetryAt { at: next_at, retries });
        }
        Some(Receipt::Rejected { reason }) => e.set_refresh_state(RefreshState::Rejected(reason)),
        Some(Receipt::Unknown) => e.set_refresh_state(RefreshState::Unknown),
        None if e.journal.has_claim(n) => e.set_refresh_state(RefreshState::Unknown),
        None => {}
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample_creds(provider: Provider, identity: &str, generation: u64) -> Credentials {
        Credentials {
            id: account_id(provider, identity),
            provider,
            identity: identity.into(),
            email: Some(format!("{identity}@example.com")),
            plan: None,
            chatgpt_account_id: None,
            grant: Grant {
                access_token: format!("at-{generation}"),
                refresh_token: format!("rt-{generation}"),
                expires_at: Utc::now() + chrono::Duration::hours(8),
                id_token: None,
            },
            generation,
            updated_at: Utc::now(),
        }
    }

    fn write_account(dir: &Path, c: &Credentials) {
        fsutil::ensure_private_dir(dir).unwrap();
        fsutil::write_json_durable(&dir.join(format!("{}.json", c.id)), c).unwrap();
    }

    #[test]
    fn ids_are_stable() {
        assert_eq!(account_id(Provider::Claude, "a:b"), account_id(Provider::Claude, "a:b"));
        assert_ne!(account_id(Provider::Claude, "a:b"), account_id(Provider::Codex, "a:b"));
    }

    #[test]
    fn recovery_applies_ok_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let c1 = sample_creds(Provider::Claude, "u1", 1);
        write_account(dir.path(), &c1);
        let j = Journal::new(dir.path(), &c1.id);
        j.write_claim(2).unwrap();
        let c2 = sample_creds(Provider::Claude, "u1", 2);
        j.write_receipt(
            2,
            &Receipt::Ok {
                credentials: Box::new(c2.clone()),
            },
        )
        .unwrap();

        let store = Store::load(dir.path()).unwrap();
        let e = store.get(&c1.id).unwrap();
        assert_eq!(e.credentials().generation, 2);
        assert_eq!(e.credentials().grant.refresh_token, "rt-2");
        assert_eq!(e.refresh_state(), RefreshState::Normal);
        assert!(!j.has_claim(2), "journal cleaned up");
    }

    #[test]
    fn recovery_bare_claim_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let c1 = sample_creds(Provider::Codex, "acct", 3);
        write_account(dir.path(), &c1);
        Journal::new(dir.path(), &c1.id).write_claim(4).unwrap();
        let store = Store::load(dir.path()).unwrap();
        assert_eq!(store.get(&c1.id).unwrap().refresh_state(), RefreshState::Unknown);
    }

    #[test]
    fn recovery_unknown_receipt_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let c1 = sample_creds(Provider::Claude, "u2", 1);
        write_account(dir.path(), &c1);
        let j = Journal::new(dir.path(), &c1.id);
        j.write_claim(2).unwrap();
        j.write_receipt(2, &Receipt::Unknown).unwrap();
        let store = Store::load(dir.path()).unwrap();
        assert_eq!(store.get(&c1.id).unwrap().refresh_state(), RefreshState::Unknown);
    }

    #[test]
    fn recovery_ignores_stale_generations() {
        let dir = tempfile::tempdir().unwrap();
        let c = sample_creds(Provider::Claude, "u3", 5);
        write_account(dir.path(), &c);
        let j = Journal::new(dir.path(), &c.id);
        j.write_claim(5).unwrap();
        j.write_receipt(5, &Receipt::Rejected { reason: "old".into() }).unwrap();
        let store = Store::load(dir.path()).unwrap();
        assert_eq!(store.get(&c.id).unwrap().refresh_state(), RefreshState::Normal);
    }

    #[test]
    fn recovery_retry_and_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = sample_creds(Provider::Claude, "ra", 1);
        let b = sample_creds(Provider::Claude, "rb", 1);
        write_account(dir.path(), &a);
        write_account(dir.path(), &b);
        let ja = Journal::new(dir.path(), &a.id);
        ja.write_claim(2).unwrap();
        let at = Utc::now();
        ja.write_receipt(
            2,
            &Receipt::Retry {
                next_at: at,
                retries: 1,
            },
        )
        .unwrap();
        let jb = Journal::new(dir.path(), &b.id);
        jb.write_claim(2).unwrap();
        jb.write_receipt(
            2,
            &Receipt::Rejected {
                reason: "invalid_grant".into(),
            },
        )
        .unwrap();
        let store = Store::load(dir.path()).unwrap();
        assert!(matches!(
            store.get(&a.id).unwrap().refresh_state(),
            RefreshState::RetryAt { retries: 1, .. }
        ));
        assert!(store.get(&b.id).unwrap().needs_login().is_some());
    }
}

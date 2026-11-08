use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};

use crate::accounts::{AccountEntry, Credentials, Grant, Provider, Receipt, RefreshState, Store};
use crate::config::Endpoints;
use crate::oauth::{self, LoginTokens, RefreshOutcome};

const CLAUDE_BACKOFF: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
];
const NOT_SENT_RETRY: Duration = Duration::from_secs(120);

pub fn refresh_lead(provider: Provider) -> chrono::Duration {
    match provider {
        Provider::Claude => chrono::Duration::hours(1),
        Provider::Codex => chrono::Duration::hours(96),
    }
}

#[derive(Debug, PartialEq)]
pub enum RefreshResult {
    Refreshed,
    Waiting(DateTime<Utc>),
    Unknown,
    Rejected(String),
    GateClosed,
    LocalError(String),
}

pub struct Refresher {
    gate: tokio::sync::RwLock<bool>,
    http: reqwest::Client,
    ep: Endpoints,
    store: Arc<Store>,
}

impl Refresher {
    pub fn new(store: Arc<Store>, ep: Endpoints, http: reqwest::Client, open: bool) -> Self {
        Self {
            gate: tokio::sync::RwLock::new(open),
            http,
            ep,
            store,
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    #[allow(dead_code)]
    pub async fn close_gate(&self) {
        *self.gate.write().await = false;
    }

    #[allow(dead_code)]
    pub async fn open_gate(&self) {
        *self.gate.write().await = true;
    }

    pub async fn refresh(self: &Arc<Self>, e: &Arc<AccountEntry>) -> RefreshResult {
        self.detached(e, None).await
    }

    pub async fn refresh_rejected(self: &Arc<Self>, e: &Arc<AccountEntry>, rejected_generation: u64) -> RefreshResult {
        self.detached(e, Some(rejected_generation)).await
    }

    async fn detached(self: &Arc<Self>, e: &Arc<AccountEntry>, rejected_generation: Option<u64>) -> RefreshResult {
        let (this, account) = (self.clone(), e.clone());
        let task = tokio::spawn(async move { this.run_refresh(&account, rejected_generation).await });
        match task.await {
            Ok(result) => result,
            Err(err) => RefreshResult::LocalError(err.to_string()),
        }
    }

    async fn run_refresh(&self, e: &Arc<AccountEntry>, rejected_generation: Option<u64>) -> RefreshResult {
        let gate = self.gate.read().await;
        if !*gate {
            return RefreshResult::GateClosed;
        }
        let _op = e.op_lock.lock().await;
        if !self.store.get(&e.id).is_some_and(|current| Arc::ptr_eq(&current, e)) {
            return RefreshResult::LocalError("account was removed".into());
        }
        if rejected_generation.is_some_and(|g| g != e.credentials().generation) {
            return RefreshResult::Refreshed;
        }
        let r = self.refresh_locked(e).await;
        drop(gate);
        r
    }

    async fn refresh_locked(&self, e: &AccountEntry) -> RefreshResult {
        let creds = e.credentials();
        if creds.generation == 0 {
            return RefreshResult::LocalError("account has never logged in".into());
        }
        let n = creds.generation + 1;

        let mut retries = 0u32;
        match e.journal.read_receipt(n) {
            Ok(None) => {
                if e.journal.has_claim(n) {
                    e.set_refresh_state(RefreshState::Unknown);
                    return RefreshResult::Unknown;
                }
            }
            Ok(Some(Receipt::Retry { next_at, retries: r })) => {
                if Utc::now() < next_at {
                    e.set_refresh_state(RefreshState::RetryAt {
                        at: next_at,
                        retries: r,
                    });
                    return RefreshResult::Waiting(next_at);
                }
                retries = r;
            }
            Ok(Some(Receipt::Unknown)) => {
                e.set_refresh_state(RefreshState::Unknown);
                return RefreshResult::Unknown;
            }
            Ok(Some(Receipt::Rejected { reason })) => {
                e.set_refresh_state(RefreshState::Rejected(reason.clone()));
                return RefreshResult::Rejected(reason);
            }
            Ok(Some(Receipt::Ok { credentials })) => {
                return match e.apply_ok(*credentials) {
                    Ok(()) => RefreshResult::Refreshed,
                    Err(err) => RefreshResult::LocalError(err.to_string()),
                };
            }
            Err(err) => return RefreshResult::LocalError(format!("journal unreadable: {err}")),
        }

        if !e.journal.has_claim(n)
            && let Err(err) = e.journal.write_claim(n)
        {
            return RefreshResult::LocalError(err.to_string());
        }
        if let Err(err) = e.journal.write_receipt(n, &Receipt::Unknown) {
            return RefreshResult::LocalError(err.to_string());
        }

        let outcome = oauth::refresh(&self.http, &self.ep, e.provider, &creds.grant.refresh_token).await;
        tracing::info!(account = %e.id, generation = n, outcome = outcome_name(&outcome), "refresh finished");

        match outcome {
            RefreshOutcome::Ok(t) => {
                let mut next = creds.clone();
                next.generation = n;
                next.updated_at = Utc::now();
                next.grant = Grant {
                    access_token: t.access_token,
                    refresh_token: t.refresh_token.unwrap_or(creds.grant.refresh_token.clone()),
                    expires_at: Utc::now() + chrono::Duration::seconds(t.expires_in),
                    id_token: t.id_token.clone().or(creds.grant.id_token.clone()),
                };
                if e.provider == Provider::Codex
                    && let Some(idt) = &t.id_token
                {
                    let ident = oauth::codex_identity(idt);
                    next.email = ident.email.or(next.email);
                    next.plan = ident.plan_type.or(next.plan);
                }
                if let Err(err) = e.journal.write_receipt(
                    n,
                    &Receipt::Ok {
                        credentials: Box::new(next.clone()),
                    },
                ) {
                    tracing::error!(account = %e.id, "could not write ok receipt: {err}");
                    return RefreshResult::LocalError(err.to_string());
                }
                if let Err(err) = e.apply_ok(next) {
                    tracing::error!(account = %e.id, "could not write account file: {err}");
                    return RefreshResult::LocalError(err.to_string());
                }
                RefreshResult::Refreshed
            }
            RefreshOutcome::Rejected(reason) => {
                let _ = e
                    .journal
                    .write_receipt(n, &Receipt::Rejected { reason: reason.clone() });
                e.set_refresh_state(RefreshState::Rejected(reason.clone()));
                RefreshResult::Rejected(reason)
            }
            RefreshOutcome::RetryAfter(hint) if e.provider == Provider::Claude => {
                let attempt = retries as usize;
                if attempt >= CLAUDE_BACKOFF.len() {
                    e.set_refresh_state(RefreshState::Unknown);
                    return RefreshResult::Unknown;
                }
                let delay = hint.max(CLAUDE_BACKOFF[attempt]);
                self.schedule_retry(e, n, delay, retries + 1)
            }
            RefreshOutcome::NotSent(_) => self.schedule_retry(e, n, NOT_SENT_RETRY, retries),
            RefreshOutcome::RetryAfter(_) | RefreshOutcome::Unknown(_) => {
                e.set_refresh_state(RefreshState::Unknown);
                RefreshResult::Unknown
            }
        }
    }

    fn schedule_retry(&self, e: &AccountEntry, n: u64, delay: Duration, retries: u32) -> RefreshResult {
        let at = Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
        if let Err(err) = e.journal.write_receipt(n, &Receipt::Retry { next_at: at, retries }) {
            tracing::error!(account = %e.id, "could not write retry receipt: {err}");
            e.set_refresh_state(RefreshState::Unknown);
            return RefreshResult::Unknown;
        }
        e.set_refresh_state(RefreshState::RetryAt { at, retries });
        RefreshResult::Waiting(at)
    }

    pub async fn install_login(&self, t: LoginTokens) -> Result<Arc<AccountEntry>> {
        let gate = self.gate.read().await;
        if !*gate {
            bail!("the hub is moving; logins are paused");
        }
        let e = self.store.entry_for_login(t.provider, &t.identity)?;
        let _op = e.op_lock.lock().await;
        let old = e.credentials();
        let n = old.generation + 1;
        let creds = Credentials {
            id: e.id.clone(),
            provider: t.provider,
            identity: t.identity,
            email: t.email.or(old.email),
            plan: t.plan.or(old.plan),
            chatgpt_account_id: t.chatgpt_account_id.or(old.chatgpt_account_id),
            grant: Grant {
                access_token: t.access_token,
                refresh_token: t.refresh_token,
                expires_at: t.expires_at,
                id_token: t.id_token,
            },
            generation: n,
            updated_at: Utc::now(),
        };
        let result = e
            .journal
            .write_receipt(
                n,
                &Receipt::Ok {
                    credentials: Box::new(creds.clone()),
                },
            )
            .and_then(|_| e.apply_ok(creds));
        drop(_op);
        drop(gate);
        if let Err(err) = result {
            self.store.forget_if_unsaved(&e.id);
            return Err(err);
        }
        Ok(e)
    }

    pub fn due(e: &AccountEntry, now: DateTime<Utc>) -> bool {
        let c = e.credentials();
        if c.generation == 0 {
            return false;
        }
        match e.refresh_state() {
            RefreshState::Rejected(_) | RefreshState::Unknown => false,
            RefreshState::RetryAt { at, .. } => now >= at,
            RefreshState::Normal => c.grant.expires_at - now < refresh_lead(e.provider),
        }
    }

    pub async fn run_scheduler(self: Arc<Self>) {
        loop {
            let open = *self.gate.read().await;
            let now = Utc::now();
            for e in self.store.all().into_iter().filter(|_| open) {
                if Self::due(&e, now) {
                    let r = self.refresh(&e).await;
                    if r != RefreshResult::Refreshed {
                        tracing::warn!(account = %e.id, result = ?r, "scheduled refresh did not complete");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

fn outcome_name(o: &RefreshOutcome) -> &'static str {
    match o {
        RefreshOutcome::Ok(_) => "ok",
        RefreshOutcome::Rejected(_) => "rejected",
        RefreshOutcome::RetryAfter(_) => "retry_after",
        RefreshOutcome::NotSent(_) => "not_sent",
        RefreshOutcome::Unknown(_) => "unknown",
    }
}

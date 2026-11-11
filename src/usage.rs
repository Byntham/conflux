use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{AccountEntry, Provider, Store};
use crate::config::Endpoints;

pub const STALE_AFTER: chrono::Duration = chrono::Duration::minutes(30);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub key: String,
    pub used_pct: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<i64>,
    pub model_scope: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub windows: Vec<Window>,
    pub overage_in_use: bool,
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub polled_at: Option<DateTime<Utc>>,
}

impl Snapshot {
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.updated_at.is_some_and(|t| now - t < STALE_AFTER)
    }

    fn upsert(&mut self, w: Window) {
        match self.windows.iter_mut().find(|x| x.key == w.key) {
            Some(x) => *x = w,
            None => self.windows.push(w),
        }
    }

    pub fn shared_windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter(|w| w.model_scope.is_none())
    }

    pub fn model_window(&self, model: &str) -> Option<&Window> {
        let words = |s: &str| -> Vec<String> {
            s.to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .map(String::from)
                .collect()
        };
        let model = words(model);
        let names_model = |name: Vec<String>| {
            name.iter().any(|w| w != "claude" && w.chars().all(char::is_alphabetic))
                && model.windows(name.len()).any(|run| run == name.as_slice())
        };
        self.windows
            .iter()
            .filter(|w| w.model_scope.as_deref().map(words).is_some_and(names_model))
            .max_by(|a, b| a.used_pct.total_cmp(&b.used_pct))
    }

    pub fn longest(&self) -> Option<&Window> {
        self.shared_windows().max_by_key(|w| w.duration_secs.unwrap_or(0))
    }

    pub fn exhausted(&self, now: DateTime<Utc>) -> bool {
        self.shared_windows()
            .any(|w| w.used_pct >= 100.0 && w.resets_at.is_none_or(|r| r > now))
    }
}

fn parse_reset(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(n) = raw.parse::<f64>() {
        let secs = if n > 1e12 { n / 1000.0 } else { n };
        return Utc.timestamp_opt(secs as i64, 0).single();
    }
    DateTime::parse_from_rfc3339(raw).ok().map(|d| d.with_timezone(&Utc))
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn apply_claude_headers(s: &mut Snapshot, h: &HeaderMap, now: DateTime<Utc>) -> bool {
    let mut touched = false;
    for (tag, key, dur) in [("5h", "five_hour", 5 * 3600), ("7d", "seven_day", 7 * 86400)] {
        let util =
            header(h, &format!("anthropic-ratelimit-unified-{tag}-utilization")).and_then(|v| v.parse::<f64>().ok());
        let reset = header(h, &format!("anthropic-ratelimit-unified-{tag}-reset")).and_then(parse_reset);
        if let Some(u) = util {
            s.upsert(Window {
                key: key.into(),
                used_pct: (u * 100.0).clamp(0.0, 100.0),
                resets_at: reset,
                duration_secs: Some(dur),
                model_scope: None,
            });
            touched = true;
        }
    }
    let claim = header(h, "anthropic-ratelimit-unified-representative-claim")
        .unwrap_or("")
        .to_lowercase();
    let overage = header(h, "anthropic-ratelimit-unified-overage-status")
        .unwrap_or("")
        .to_lowercase();
    if !claim.is_empty() || !overage.is_empty() {
        s.overage_in_use = claim.contains("overage") && (overage == "allowed" || overage == "allowed_warning");
        touched = true;
    }
    if touched {
        s.updated_at = Some(now);
    }
    touched
}

pub fn apply_codex_headers(s: &mut Snapshot, h: &HeaderMap, now: DateTime<Utc>) -> bool {
    let mut touched = false;
    for key in ["primary", "secondary"] {
        let Some(used) = header(h, &format!("x-codex-{key}-used-percent")).and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        let minutes = header(h, &format!("x-codex-{key}-window-minutes")).and_then(|v| v.parse::<i64>().ok());
        let reset = header(h, &format!("x-codex-{key}-reset-at"))
            .and_then(parse_reset)
            .or_else(|| {
                header(h, &format!("x-codex-{key}-reset-after-seconds"))
                    .and_then(|v| v.parse::<i64>().ok())
                    .map(|secs| now + chrono::Duration::seconds(secs))
            });
        s.upsert(Window {
            key: key.into(),
            used_pct: used.clamp(0.0, 100.0),
            resets_at: reset,
            duration_secs: minutes.map(|m| m * 60),
            model_scope: None,
        });
        touched = true;
    }
    if touched {
        s.updated_at = Some(now);
    }
    touched
}

pub fn parse_claude_usage(v: &Value, now: DateTime<Utc>) -> Snapshot {
    let mut s = Snapshot::default();
    for (key, dur) in [("five_hour", 5 * 3600), ("seven_day", 7 * 86400)] {
        let w = &v[key];
        if let Some(u) = w["utilization"].as_f64() {
            s.upsert(Window {
                key: key.into(),
                used_pct: u.clamp(0.0, 100.0),
                resets_at: w["resets_at"].as_str().and_then(parse_reset),
                duration_secs: Some(dur),
                model_scope: None,
            });
        }
    }
    if let Some(limits) = v["limits"].as_array() {
        for l in limits {
            if l["kind"].as_str() != Some("weekly_scoped") {
                continue;
            }
            let (Some(name), Some(p)) = (l["scope"]["model"]["display_name"].as_str(), l["percent"].as_f64()) else {
                continue;
            };
            s.upsert(Window {
                key: format!("weekly:{name}"),
                used_pct: p.clamp(0.0, 100.0),
                resets_at: l["resets_at"].as_str().and_then(parse_reset),
                duration_secs: Some(7 * 86400),
                model_scope: Some(name.to_string()),
            });
        }
    }
    s.updated_at = Some(now);
    s.polled_at = Some(now);
    s
}

pub fn parse_codex_usage(v: &Value, now: DateTime<Utc>) -> Snapshot {
    let mut s = Snapshot::default();
    for (key, field) in [("primary", "primary_window"), ("secondary", "secondary_window")] {
        let w = &v["rate_limit"][field];
        if let Some(u) = w["used_percent"].as_f64() {
            s.upsert(Window {
                key: key.into(),
                used_pct: u.clamp(0.0, 100.0),
                resets_at: w["reset_at"]
                    .as_f64()
                    .and_then(|n| Utc.timestamp_opt(n as i64, 0).single()),
                duration_secs: w["limit_window_seconds"].as_i64(),
                model_scope: None,
            });
        }
    }
    s.updated_at = Some(now);
    s.polled_at = Some(now);
    s
}

pub struct Usage {
    snaps: RwLock<HashMap<String, Snapshot>>,
    last_used: RwLock<HashMap<String, DateTime<Utc>>>,
    paused: AtomicBool,
    http: reqwest::Client,
    ep: Endpoints,
}

impl Usage {
    pub fn new(ep: Endpoints, http: reqwest::Client) -> Self {
        Self {
            snaps: RwLock::default(),
            last_used: RwLock::default(),
            paused: AtomicBool::new(false),
            http,
            ep,
        }
    }

    pub fn export(&self) -> Value {
        json!({ "snaps": *self.snaps.read().unwrap(), "last_used": *self.last_used.read().unwrap() })
    }

    pub fn import(&self, state: &Value) {
        if let Ok(snaps) = serde_json::from_value(state["snaps"].clone()) {
            *self.snaps.write().unwrap() = snaps;
        }
        if let Ok(last_used) = serde_json::from_value(state["last_used"].clone()) {
            *self.last_used.write().unwrap() = last_used;
        }
    }

    pub fn get(&self, id: &str) -> Snapshot {
        self.snaps.read().unwrap().get(id).cloned().unwrap_or_default()
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn mark_used(&self, id: &str) {
        self.last_used.write().unwrap().insert(id.to_string(), Utc::now());
    }

    pub fn invalidate(&self, id: &str) {
        if let Some(s) = self.snaps.write().unwrap().get_mut(id) {
            s.updated_at = None;
        }
    }

    pub fn observe_headers(&self, id: &str, provider: Provider, h: &HeaderMap) {
        let now = Utc::now();
        let mut snaps = self.snaps.write().unwrap();
        let s = snaps.entry(id.to_string()).or_default();
        match provider {
            Provider::Claude => apply_claude_headers(s, h, now),
            Provider::Codex => apply_codex_headers(s, h, now),
        };
    }

    #[cfg(test)]
    pub fn set(&self, id: &str, s: Snapshot) {
        self.snaps.write().unwrap().insert(id.to_string(), s);
    }

    pub async fn poll(&self, e: &AccountEntry) -> anyhow::Result<()> {
        let creds = e.credentials();
        let req = match e.provider {
            Provider::Claude => self
                .http
                .get(format!("{}/api/oauth/usage", self.ep.anthropic_api))
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("User-Agent", "claude-code/2.1.273"),
            Provider::Codex => {
                let mut r = self
                    .http
                    .get(format!("{}/wham/usage", self.ep.chatgpt_backend))
                    .header("OpenAI-Beta", "codex-1")
                    .header("Originator", "codex_cli_rs");
                if let Some(acct) = &creds.chatgpt_account_id {
                    r = r.header("Chatgpt-Account-Id", acct);
                }
                r
            }
        };
        let resp = req.bearer_auth(&creds.grant.access_token).send().await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("usage endpoint returned HTTP {status}");
        }
        let now = Utc::now();
        let mut snap = match e.provider {
            Provider::Claude => parse_claude_usage(&body, now),
            Provider::Codex => parse_codex_usage(&body, now),
        };
        if let Some(old) = self.snaps.read().unwrap().get(&e.id) {
            snap.overage_in_use = old.overage_in_use && snap.exhausted(now);
        }
        self.snaps.write().unwrap().insert(e.id.clone(), snap);
        Ok(())
    }

    pub async fn run_poller(self: Arc<Self>, store: Arc<Store>) {
        loop {
            let now = Utc::now();
            let paused = self.paused.load(Ordering::SeqCst);
            for e in store.all().into_iter().filter(|_| !paused) {
                if e.credentials().generation == 0 || e.needs_login().is_some() {
                    continue;
                }
                let snap = self.get(&e.id);
                let idle = self
                    .last_used
                    .read()
                    .unwrap()
                    .get(&e.id)
                    .is_none_or(|t| now - *t > chrono::Duration::minutes(10));
                let old = snap.updated_at.is_none_or(|t| now - t > chrono::Duration::minutes(15));
                if (snap.updated_at.is_none() || (idle && old))
                    && let Err(err) = self.poll(&e).await
                {
                    tracing::warn!(account = %e.id, "usage poll failed: {err}");
                }
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn claude_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("0.42"),
        );
        h.insert(
            "anthropic-ratelimit-unified-5h-reset",
            HeaderValue::from_static("1900000000"),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_static("1.0"),
        );
        let mut s = Snapshot::default();
        assert!(apply_claude_headers(&mut s, &h, Utc::now()));
        let five = s.windows.iter().find(|w| w.key == "five_hour").unwrap();
        assert!((five.used_pct - 42.0).abs() < 1e-9);
        assert_eq!(five.resets_at.unwrap().timestamp(), 1_900_000_000);
        assert_eq!(s.longest().unwrap().key, "seven_day");
        assert!(s.exhausted(Utc::now()));
    }

    #[test]
    fn codex_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-codex-primary-used-percent", HeaderValue::from_static("12.5"));
        h.insert("x-codex-primary-window-minutes", HeaderValue::from_static("300"));
        h.insert("x-codex-primary-reset-after-seconds", HeaderValue::from_static("60"));
        h.insert("x-codex-secondary-used-percent", HeaderValue::from_static("80"));
        h.insert("x-codex-secondary-window-minutes", HeaderValue::from_static("10080"));
        let mut s = Snapshot::default();
        apply_codex_headers(&mut s, &h, Utc::now());
        assert_eq!(s.longest().unwrap().key, "secondary");
        assert!(!s.exhausted(Utc::now()));
    }

    #[test]
    fn usage_endpoints() {
        let now = Utc::now();
        let c = serde_json::json!({
            "five_hour": {"utilization": 10.0, "resets_at": "2030-01-01T00:00:00Z"},
            "seven_day": {"utilization": 55.0, "resets_at": "2030-01-05T00:00:00Z"},
            "limits": [{"kind": "weekly_scoped", "percent": 90.0, "resets_at": "2030-01-05T00:00:00Z",
                        "scope": {"model": {"display_name": "Fable"}}}],
            "extra_usage": {"is_enabled": false}
        });
        let s = parse_claude_usage(&c, now);
        assert_eq!(s.windows.len(), 3);
        assert_eq!(s.shared_windows().count(), 2);

        let x = serde_json::json!({"rate_limit": {
            "primary_window": {"used_percent": 5, "reset_at": 1900000000, "limit_window_seconds": 18000},
            "secondary_window": {"used_percent": 70, "reset_at": 1900500000, "limit_window_seconds": 604800}
        }});
        let s = parse_codex_usage(&x, now);
        assert_eq!(s.longest().unwrap().duration_secs, Some(604800));
    }

    #[test]
    fn model_windows_match_whole_names_in_order() {
        let windows = |name: &str| Snapshot {
            windows: vec![Window {
                key: format!("weekly:{name}"),
                used_pct: 0.0,
                resets_at: None,
                duration_secs: None,
                model_scope: Some(name.into()),
            }],
            ..Snapshot::default()
        };
        for (name, model, matches) in [
            ("Fable", "claude-fable-5-1", true),
            ("Claude Fable 5.1", "claude-fable-5-1", true),
            ("Sonnet 4", "claude-sonnet-4-5", true),
            ("Sonnet 5", "claude-sonnet-4-5", false),
            ("Opus", "claude-fable-5-1", false),
            ("Claude", "claude-fable-5-1", false),
            ("5.1", "claude-fable-5-1", false),
            ("", "claude-fable-5-1", false),
        ] {
            assert_eq!(
                windows(name).model_window(model).is_some(),
                matches,
                "{name} vs {model}"
            );
        }
    }
}

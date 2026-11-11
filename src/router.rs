use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::accounts::{AccountEntry, Provider, Store};
use crate::usage::{Snapshot, Usage};

const PIN_TTL: chrono::Duration = chrono::Duration::hours(1);
const MAX_WINDOW: chrono::Duration = chrono::Duration::hours(7 * 24 + 1);
const ACCOUNT_SCOPE: &str = "account";
const MODEL_SCOPE: &str = "model:";

pub fn model_scope(model: &str) -> String {
    format!("{MODEL_SCOPE}{model}")
}

#[derive(Clone, Debug)]
pub struct RouteRequest {
    pub provider: Provider,
    pub model: Option<String>,
    pub session: Option<String>,
    pub previous_response_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cooldown {
    pub scope: String,
    pub until: DateTime<Utc>,
    pub reason: String,
    #[serde(default = "Utc::now")]
    at: DateTime<Utc>,
}

impl Cooldown {
    fn lifted_by(&self, snap: &Snapshot, now: DateTime<Utc>) -> bool {
        let scope_has_room = match self.scope.strip_prefix(MODEL_SCOPE) {
            Some(model) => {
                snap.polled_at.is_some_and(|t| t > self.at)
                    && snap.model_window(model).is_some_and(|w| w.used_pct < 100.0)
            }
            None => self.scope == ACCOUNT_SCOPE,
        };
        scope_has_room && has_room_since(snap, self.at, now)
    }
}

#[derive(Serialize, Deserialize)]
struct BillingBlock {
    reason: String,
    at: DateTime<Utc>,
}

impl BillingBlock {
    fn cleared_by(&self, snap: &Snapshot, now: DateTime<Utc>) -> bool {
        has_room_since(snap, self.at, now)
    }
}

fn has_room_since(snap: &Snapshot, since: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    snap.updated_at.is_some_and(|t| t > since) && snap.is_fresh(now) && !snap.exhausted(now)
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    cooldowns: HashMap<String, HashMap<String, Cooldown>>,
    pins: HashMap<String, (String, DateTime<Utc>)>,
    billing_blocked: HashMap<String, BillingBlock>,
    response_owner: HashMap<String, (String, DateTime<Utc>)>,
    #[serde(skip)]
    successor: Option<mpsc::UnboundedSender<Value>>,
}

pub struct Router {
    store: Arc<Store>,
    usage: Arc<Usage>,
    state: Mutex<State>,
}

#[derive(Debug, PartialEq)]
pub enum NoAccount {
    NoneConfigured,
    AllLimited(Option<DateTime<Utc>>),
}

impl Router {
    pub fn new(store: Arc<Store>, usage: Arc<Usage>) -> Arc<Self> {
        Arc::new(Self {
            store,
            usage,
            state: Mutex::default(),
        })
    }

    fn cooling(st: &State, id: &str, model: Option<&str>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let cds = st.cooldowns.get(id)?;
        let mut until = None;
        for c in cds.values() {
            let applies = c.scope == ACCOUNT_SCOPE || model.is_some_and(|m| c.scope == model_scope(m));
            if applies && c.until > now {
                until = Some(until.map_or(c.until, |u: DateTime<Utc>| u.max(c.until)));
            }
        }
        until
    }

    pub fn pick(&self, req: &RouteRequest, tried: &HashSet<String>) -> Result<Arc<AccountEntry>, NoAccount> {
        let now = Utc::now();
        let model = req.model.as_deref();
        let mut st = self.state.lock().unwrap();
        st.pins.retain(|_, (_, t)| now - *t < PIN_TTL);
        st.response_owner.retain(|_, (_, t)| now - *t < PIN_TTL);

        let mut any_configured = false;
        let mut soonest: Option<DateTime<Utc>> = None;
        let mut eligible = Vec::new();
        let mut overage_pool = Vec::new();

        for e in self.store.all() {
            if req.provider != e.provider {
                continue;
            }
            if e.credentials().generation == 0 || !e.settings().enabled || e.needs_login().is_some() {
                continue;
            }
            any_configured = true;
            if tried.contains(&e.id) {
                continue;
            }
            let snap = self.usage.get(&e.id);
            if let Some(block) = st.billing_blocked.get(&e.id) {
                if !block.cleared_by(&snap, now) {
                    continue;
                }
                st.billing_blocked.remove(&e.id);
            }
            if let Some(cds) = st.cooldowns.get_mut(&e.id) {
                cds.retain(|_, c| {
                    let lifted = c.until > now && c.lifted_by(&snap, now);
                    if lifted {
                        tracing::info!(account = %e.id, scope = %c.scope, reason = %c.reason, until = %c.until, "usage shows room again; cooldown lifted");
                    }
                    !lifted
                });
            }
            if let Some(until) = Self::cooling(&st, &e.id, model, now) {
                soonest = Some(soonest.map_or(until, |s| s.min(until)));
                continue;
            }
            let fresh = snap.is_fresh(now);
            let exhausted = fresh && snap.exhausted(now);
            if exhausted {
                let full = snap.shared_windows().filter(|w| w.used_pct >= 100.0);
                if let Some(r) = full.filter_map(|w| w.resets_at).max() {
                    soonest = Some(soonest.map_or(r, |s| s.min(r)));
                }
            }
            if exhausted || (fresh && snap.overage_in_use) {
                if e.settings().allow_overage {
                    overage_pool.push(e);
                }
                continue;
            }
            eligible.push(e);
        }

        if eligible.is_empty() {
            eligible = overage_pool;
        }
        if eligible.is_empty() {
            return Err(if !any_configured {
                NoAccount::NoneConfigured
            } else {
                NoAccount::AllLimited(soonest)
            });
        }

        let find = |id: &str| eligible.iter().find(|e| e.id == id).cloned();
        let pin_key = req.session.as_ref().map(|s| format!("{}:{s}", req.provider.as_str()));

        let chosen = req
            .previous_response_id
            .as_ref()
            .and_then(|r| st.response_owner.get(r).map(|(a, _)| a.clone()))
            .and_then(|a| find(&a))
            .or_else(|| pin_key.as_ref().and_then(|k| st.pins.get(k)).and_then(|(a, _)| find(a)))
            .unwrap_or_else(|| {
                let far = DateTime::<Utc>::MAX_UTC;
                let mut ranked: Vec<_> = eligible
                    .iter()
                    .map(|e| {
                        let s = self.usage.get(&e.id);
                        let fresh = s.is_fresh(now);
                        let reset = if fresh {
                            s.longest()
                                .and_then(|w| w.resets_at)
                                .filter(|r| *r > now)
                                .unwrap_or(far)
                        } else {
                            far
                        };
                        (!fresh, reset, e.id.clone(), e.clone())
                    })
                    .collect();
                ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
                ranked.remove(0).3
            });

        if let Some(k) = pin_key {
            st.pins.insert(k, (chosen.id.clone(), now));
        }
        drop(st);
        self.usage.mark_used(&chosen.id);
        Ok(chosen)
    }

    pub fn export(&self, successor: mpsc::UnboundedSender<Value>) -> Value {
        let mut st = self.state.lock().unwrap();
        st.successor = Some(successor);
        serde_json::to_value(&*st).unwrap_or_default()
    }

    pub fn import_later_owner(&self, owner: &Value) {
        if let Ok((response_id, account)) = serde_json::from_value::<(String, String)>(owner.clone()) {
            self.record_response_owner(&response_id, &account);
        }
    }

    pub fn import(&self, state: &Value) {
        match serde_json::from_value(state.clone()) {
            Ok(state) => *self.state.lock().unwrap() = state,
            Err(e) => tracing::warn!("could not carry over routing state: {e}"),
        }
    }

    pub fn cool(&self, account: &str, scope: &str, until: DateTime<Utc>, reason: &str) {
        let now = Utc::now();
        let mut st = self.state.lock().unwrap();
        if until - now > MAX_WINDOW {
            tracing::warn!(account, %until, "reset is past the longest window; blocking for billing");
            st.billing_blocked.insert(
                account.to_string(),
                BillingBlock {
                    reason: format!("{reason} (resets {until})"),
                    at: now,
                },
            );
            return;
        }
        tracing::info!(account, scope, %until, reason, "cooldown");
        st.cooldowns.entry(account.to_string()).or_default().insert(
            scope.to_string(),
            Cooldown {
                scope: scope.to_string(),
                until,
                reason: reason.to_string(),
                at: now,
            },
        );
    }

    pub fn record_response_owner(&self, response_id: &str, account: &str) {
        let mut st = self.state.lock().unwrap();
        st.response_owner
            .insert(response_id.to_string(), (account.to_string(), Utc::now()));
        if let Some(successor) = &st.successor {
            let _ = successor.send(json!({ "owner": [response_id, account] }));
        }
    }

    pub fn clear(&self, account: &str) {
        let mut st = self.state.lock().unwrap();
        st.cooldowns.remove(account);
        st.billing_blocked.remove(account);
    }

    pub fn cooldowns(&self, account: &str) -> Vec<Cooldown> {
        let snap = self.usage.get(account);
        let now = Utc::now();
        let st = self.state.lock().unwrap();
        st.cooldowns
            .get(account)
            .map(|m| {
                m.values()
                    .filter(|c| c.until > now && !c.lifted_by(&snap, now))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn billing_blocked(&self, account: &str) -> Option<String> {
        let snap = self.usage.get(account);
        let now = Utc::now();
        let st = self.state.lock().unwrap();
        let block = st.billing_blocked.get(account)?;
        (!block.cleared_by(&snap, now)).then(|| block.reason.clone())
    }
}

pub const ACCOUNT: &str = ACCOUNT_SCOPE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Receipt;
    use crate::accounts::tests::sample_creds;
    use crate::config::Endpoints;
    use crate::usage::{Snapshot, Window};

    fn setup(ids: &[(&str, Provider)]) -> (tempfile::TempDir, Arc<Store>, Arc<Usage>, Arc<Router>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()).unwrap());
        for (ident, p) in ids {
            let c = sample_creds(*p, ident, 1);
            let e = store.entry_for_login(*p, ident).unwrap();
            e.journal
                .write_receipt(
                    1,
                    &Receipt::Ok {
                        credentials: Box::new(c.clone()),
                    },
                )
                .unwrap();
            e.apply_ok(c).unwrap();
        }
        let usage = Arc::new(Usage::new(Endpoints::default(), reqwest::Client::new()));
        let router = Router::new(store.clone(), usage.clone());
        (dir, store, usage, router)
    }

    fn snap(seven_day_reset_h: i64, five_pct: f64, seven_pct: f64) -> Snapshot {
        let now = Utc::now();
        Snapshot {
            windows: vec![
                Window {
                    key: "five_hour".into(),
                    used_pct: five_pct,
                    resets_at: Some(now + chrono::Duration::hours(2)),
                    duration_secs: Some(5 * 3600),
                    model_scope: None,
                },
                Window {
                    key: "seven_day".into(),
                    used_pct: seven_pct,
                    resets_at: Some(now + chrono::Duration::hours(seven_day_reset_h)),
                    duration_secs: Some(7 * 86400),
                    model_scope: None,
                },
            ],
            overage_in_use: false,
            updated_at: Some(now),
            polled_at: Some(now),
        }
    }

    fn route(provider: Provider) -> RouteRequest {
        RouteRequest {
            provider,
            model: None,
            session: None,
            previous_response_id: None,
        }
    }

    fn id(p: Provider, ident: &str) -> String {
        crate::accounts::account_id(p, ident)
    }

    #[test]
    fn picks_soonest_weekly_reset() {
        let (_d, _s, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        usage.set(&id(Provider::Claude, "a"), snap(100, 10.0, 40.0));
        usage.set(&id(Provider::Claude, "b"), snap(20, 50.0, 60.0));
        let req = route(Provider::Claude);
        let account = router.pick(&req, &HashSet::new()).unwrap();
        assert_eq!(account.id, id(Provider::Claude, "b"));
    }

    #[test]
    fn a_reset_that_already_passed_does_not_count_as_soonest() {
        let (_d, _s, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        usage.set(&id(Provider::Claude, "a"), snap(-1, 10.0, 40.0));
        usage.set(&id(Provider::Claude, "b"), snap(24, 10.0, 40.0));
        let req = route(Provider::Claude);
        assert_eq!(
            router.pick(&req, &HashSet::new()).unwrap().id,
            id(Provider::Claude, "b")
        );
    }

    #[test]
    fn equal_resets_fall_back_to_the_account_id() {
        let (_d, _s, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        let (a, b) = (id(Provider::Claude, "a"), id(Provider::Claude, "b"));
        let (lower_id, higher_id) = if a < b { (a, b) } else { (b, a) };
        let busy = snap(10, 90.0, 40.0);
        let mut idle = busy.clone();
        idle.windows[0].used_pct = 10.0;
        usage.set(&lower_id, busy);
        usage.set(&higher_id, idle);
        let req = route(Provider::Claude);
        assert_eq!(router.pick(&req, &HashSet::new()).unwrap().id, lower_id);
    }

    #[test]
    fn skips_exhausted_and_unknown_sorts_last() {
        let (_d, _s, usage, router) = setup(&[
            ("a", Provider::Claude),
            ("b", Provider::Claude),
            ("c", Provider::Claude),
        ]);
        usage.set(&id(Provider::Claude, "a"), snap(10, 100.0, 50.0));
        usage.set(&id(Provider::Claude, "b"), snap(90, 10.0, 10.0));
        let req = route(Provider::Claude);
        assert_eq!(
            router.pick(&req, &HashSet::new()).unwrap().id,
            id(Provider::Claude, "b")
        );
        let tried: HashSet<_> = [id(Provider::Claude, "b")].into();
        assert_eq!(router.pick(&req, &tried).unwrap().id, id(Provider::Claude, "c"));
    }

    #[test]
    fn a_session_stays_pinned_when_the_ranking_changes() {
        let (_d, _s, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        usage.set(&id(Provider::Claude, "a"), snap(100, 10.0, 40.0));
        usage.set(&id(Provider::Claude, "b"), snap(20, 50.0, 60.0));
        let req = RouteRequest {
            session: Some("s1".into()),
            ..route(Provider::Claude)
        };
        let first = router.pick(&req, &HashSet::new()).unwrap().id.clone();
        usage.set(&id(Provider::Claude, "a"), snap(1, 0.0, 0.0));
        assert_eq!(router.pick(&req, &HashSet::new()).unwrap().id, first);
    }

    #[test]
    fn continuation_follows_response_owner() {
        let (_d, _s, usage, router) = setup(&[("a", Provider::Codex), ("b", Provider::Codex)]);
        let a = id(Provider::Codex, "a");
        let b = id(Provider::Codex, "b");
        usage.set(&a, snap(100, 10.0, 10.0));
        usage.set(&b, snap(1, 10.0, 10.0));
        router.record_response_owner("resp_1", &a);
        let req = RouteRequest {
            previous_response_id: Some("resp_1".into()),
            ..route(Provider::Codex)
        };
        assert_eq!(router.pick(&req, &HashSet::new()).unwrap().id, a);
        let tried: HashSet<_> = [a.clone()].into();
        assert_eq!(router.pick(&req, &tried).unwrap().id, b);
    }

    #[test]
    fn scoped_cooldown_only_blocks_that_model() {
        let (_d, _s, _u, router) = setup(&[("a", Provider::Claude)]);
        let a = id(Provider::Claude, "a");
        router.cool(
            &a,
            &model_scope("claude-fable-5-1"),
            Utc::now() + chrono::Duration::hours(1),
            "fable weekly",
        );
        let fable = RouteRequest {
            model: Some("claude-fable-5-1".into()),
            ..route(Provider::Claude)
        };
        let sonnet = RouteRequest {
            model: Some("claude-sonnet-5-5".into()),
            ..route(Provider::Claude)
        };
        assert!(matches!(
            router.pick(&fable, &HashSet::new()),
            Err(NoAccount::AllLimited(Some(_)))
        ));
        assert!(router.pick(&sonnet, &HashSet::new()).is_ok());
        router.clear(&a);
        assert!(router.pick(&fable, &HashSet::new()).is_ok());
    }

    #[test]
    fn a_cooldown_handed_to_the_next_process_keeps_blocking() {
        let (_d, store, usage, router) = setup(&[("a", Provider::Codex)]);
        let a = id(Provider::Codex, "a");
        usage.set(&a, snap(100, 10.0, 10.0));
        std::thread::sleep(std::time::Duration::from_millis(2));
        router.cool(&a, ACCOUNT, Utc::now() + chrono::Duration::days(2), "Codex usage limit");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let next = Router::new(store, usage);
        next.import(&router.export(tx));
        assert!(next.pick(&route(Provider::Codex), &HashSet::new()).is_err());
    }

    #[test]
    fn billing_boundary_blocks_instead_of_cooling() {
        let (_d, _s, _u, router) = setup(&[("a", Provider::Claude)]);
        let a = id(Provider::Claude, "a");
        router.cool(&a, ACCOUNT, Utc::now() + chrono::Duration::days(20), "quota");
        assert!(router.billing_blocked(&a).is_some());
        let req = route(Provider::Claude);
        assert_eq!(
            router.pick(&req, &HashSet::new()).err(),
            Some(NoAccount::AllLimited(None))
        );
    }

    #[test]
    fn overage_only_when_allowed_and_nothing_else() {
        let (_d, store, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        let a = id(Provider::Claude, "a");
        let b = id(Provider::Claude, "b");
        let mut s = snap(10, 10.0, 10.0);
        s.overage_in_use = true;
        usage.set(&a, s);
        usage.set(&b, snap(100, 10.0, 10.0));
        let req = route(Provider::Claude);
        assert_eq!(router.pick(&req, &HashSet::new()).unwrap().id, b);
        let tried: HashSet<_> = [b.clone()].into();
        assert!(router.pick(&req, &tried).is_err());
        store
            .get(&a)
            .unwrap()
            .update_settings(|s| s.allow_overage = true)
            .unwrap();
        assert_eq!(router.pick(&req, &HashSet::new()).unwrap().id, b);
        assert_eq!(router.pick(&req, &tried).unwrap().id, a);
    }

    #[test]
    fn allowed_overage_covers_an_exhausted_account_as_a_last_resort() {
        let (_d, store, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        let a = id(Provider::Claude, "a");
        let b = id(Provider::Claude, "b");
        usage.set(&a, snap(10, 100.0, 50.0));
        usage.set(&b, snap(100, 10.0, 10.0));
        let req = route(Provider::Claude);
        let without_b: HashSet<_> = [b.clone()].into();
        assert!(router.pick(&req, &without_b).is_err());
        store
            .get(&a)
            .unwrap()
            .update_settings(|s| s.allow_overage = true)
            .unwrap();
        assert_eq!(router.pick(&req, &HashSet::new()).unwrap().id, b);
        assert_eq!(router.pick(&req, &without_b).unwrap().id, a);
    }

    #[test]
    fn a_healthy_poll_lifts_quota_blocks_after_an_early_reset() {
        let (_d, _s, usage, router) = setup(&[("a", Provider::Claude), ("b", Provider::Claude)]);
        let a = id(Provider::Claude, "a");
        let b = id(Provider::Claude, "b");
        let fable = model_scope("claude-fable-5-1");
        usage.set(&a, snap(100, 10.0, 10.0));
        usage.set(&b, snap(100, 10.0, 10.0));
        router.cool(&a, ACCOUNT, Utc::now() + chrono::Duration::days(20), "quota");
        router.cool(&b, ACCOUNT, Utc::now() + chrono::Duration::days(2), "quota");
        router.cool(&b, &fable, Utc::now() + chrono::Duration::days(2), "fable weekly");
        let req = route(Provider::Claude);
        assert!(
            router.pick(&req, &HashSet::new()).is_err(),
            "a snapshot from before the block proves nothing"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        usage.set(&b, snap(100, 10.0, 100.0));
        assert_eq!(
            router.cooldowns(&b).len(),
            2,
            "a poll that is still full keeps the block"
        );
        usage.set(&a, snap(100, 10.0, 10.0));
        usage.set(&b, snap(100, 10.0, 10.0));
        assert!(router.billing_blocked(&a).is_none());
        let without_b: HashSet<_> = [b.clone()].into();
        assert_eq!(router.pick(&req, &without_b).unwrap().id, a);
        let left: Vec<_> = router.cooldowns(&b).into_iter().map(|c| c.scope).collect();
        assert_eq!(left, vec![fable]);
        let without_a: HashSet<_> = [a.clone()].into();
        assert_eq!(router.pick(&req, &without_a).unwrap().id, b);
        let fable_req = RouteRequest {
            model: Some("claude-fable-5-1".into()),
            ..route(Provider::Claude)
        };
        assert!(router.pick(&fable_req, &without_a).is_err());
        let with_models = |fable_pct: f64| {
            let scoped = |name: &str, percent: f64| {
                serde_json::json!({"kind": "weekly_scoped", "percent": percent,
                    "scope": {"model": {"display_name": name}}})
            };
            crate::usage::parse_claude_usage(
                &serde_json::json!({
                    "five_hour": {"utilization": 10.0},
                    "seven_day": {"utilization": 10.0},
                    "limits": [scoped("Opus", 0.0), scoped("Fable", fable_pct)],
                }),
                Utc::now(),
            )
        };
        let mut polled_before_the_cap = with_models(0.0);
        polled_before_the_cap.polled_at = Some(Utc::now() - chrono::Duration::hours(1));
        usage.set(&b, polled_before_the_cap);
        let mut healthy = http::HeaderMap::new();
        for tag in ["5h", "7d"] {
            healthy.insert(
                http::HeaderName::try_from(format!("anthropic-ratelimit-unified-{tag}-utilization")).unwrap(),
                http::HeaderValue::from_static("0.1"),
            );
        }
        usage.observe_headers(&b, Provider::Claude, &healthy);
        assert!(
            router.pick(&fable_req, &without_a).is_err(),
            "headers carry no model window, so they cannot lift a model cooldown"
        );
        usage.set(&b, with_models(100.0));
        assert!(
            router.pick(&fable_req, &without_a).is_err(),
            "another model's window refilling says nothing about Fable"
        );
        usage.set(&b, with_models(0.0));
        assert_eq!(router.pick(&fable_req, &without_a).unwrap().id, b);
        assert!(router.cooldowns(&b).is_empty());
    }

    #[test]
    fn provider_filter_and_none_configured() {
        let (_d, _s, _u, router) = setup(&[("a", Provider::Claude)]);
        let req = route(Provider::Codex);
        assert_eq!(
            router.pick(&req, &HashSet::new()).err(),
            Some(NoAccount::NoneConfigured)
        );
    }
}

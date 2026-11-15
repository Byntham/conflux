use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Value, json};

use crate::api::account_view;
use crate::app::App;
use crate::tailnet::Caller;
use crate::update;

const PAGE: &str = include_str!("dashboard.html");
const INSTALL_BASE: &str = "https://raw.githubusercontent.com/Byntham/conflux/main";

pub async fn page() -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE)).into_response()
}

pub fn hub_name(app: &App, request_host: Option<&str>) -> String {
    let requested = request_host
        .and_then(|h| h.rsplit_once(':').map(|(name, _)| name).or(Some(h)))
        .unwrap_or("");
    if requested.is_empty() || ["127.0.0.1", "localhost", "[::1]"].contains(&requested) {
        return app.network_name();
    }
    requested.to_string()
}

pub fn install_unless_running(port: u16) -> String {
    format!(
        "sh -c 'if curl -s -m 3 --noproxy \"*\" -o /dev/null http://127.0.0.1:{port}/; then \
         echo \"conflux already runs there, so the hub left it alone\" >&2; exit 1; fi; \
         installer=$(curl -fsSL {INSTALL_BASE}/install.sh) || exit 1; \
         printf \"%s\\n\" \"$installer\" | sh'"
    )
}

pub fn add_machine(hub: &str) -> Value {
    let command = format!("curl -fsSL {INSTALL_BASE}/install.sh | sh -s -- {hub}");
    let windows_command = format!("irm {INSTALL_BASE}/install.ps1 | iex\nconflux join {hub}");
    let agent_prompt = format!(
        "Set up conflux on this machine so its Claude Code and Codex use my hub. Run `{command}`, \
         or on Windows run `{windows_command}` in PowerShell. The machine must be on my tailnet. \
         When it finishes, run `conflux status` and tell me whether it can reach the hub."
    );
    json!({ "command": command, "windows_command": windows_command, "agent_prompt": agent_prompt })
}

pub async fn data(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<Value> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let name = hub_name(&app, host);
    let caller = app.identify(from.ip()).await;
    let owner = caller.is_owner();
    let mut accounts: Vec<_> = app
        .store
        .all()
        .iter()
        .filter(|e| e.credentials().generation > 0)
        .map(|e| account_view(&app, e))
        .collect();
    let mut history = app.usage.history();
    history.retain(|id, _| accounts.iter().any(|a| &a.id == id));
    let mut machines = app.fleet();
    if !owner {
        for account in &mut accounts {
            account.email = None;
        }
        let own = match &caller {
            Caller::Peer(identity) => identity.name.clone(),
            Caller::Local | Caller::Unknown => None,
        };
        machines.retain(|m| m.role == "hub" || (own.is_some() && m.tailnet_name == own));
        for m in &mut machines {
            m.outdated &= m.role != "hub";
        }
    }
    Json(json!({
        "hub": { "name": name, "port": app.cfg.port, "version": update::CURRENT, "state": app.hub_state.get() },
        "guest": !owner,
        "latest_version": app.updater.latest(),
        "accounts": accounts,
        "usage_history": history,
        "machines": machines,
        "requests": if owner { json!(app.requests.recent()) } else { json!([]) },
        "add_machine": owner.then(|| add_machine(&name)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_has_no_external_assets() {
        assert!(!PAGE.contains("https://"));
        assert!(PAGE.contains("/conflux/api/dashboard"));
    }
}

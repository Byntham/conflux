use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::api::{ApiError, account_view};
use crate::app::App;
use crate::tailnet::Caller;
use crate::update;

const PAGE: &str = include_str!("dashboard.html");
const FONT: &[u8] = include_bytes!("dashboard.woff2");
const INSTALL_BASE: &str = "https://raw.githubusercontent.com/Byntham/conflux/main";

pub async fn page() -> Response {
    let page = PAGE.replace("__VERSION__", update::CURRENT);
    ([(header::CACHE_CONTROL, "no-store")], Html(page)).into_response()
}

pub async fn font() -> Response {
    let named = [
        (header::CONTENT_TYPE, "font/woff2"),
        (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
    ];
    (named, FONT).into_response()
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
        "Set up conflux on this machine so its coding tools use my hub. Run `{command}`, \
         or on Windows run `{windows_command}` in PowerShell. The machine must be on my tailnet. \
         When it finishes, run `conflux status` and tell me whether it can reach the hub."
    );
    json!({ "command": command, "windows_command": windows_command, "agent_prompt": agent_prompt })
}

async fn admitted(app: &App, from: SocketAddr) -> Result<Caller, ApiError> {
    match app.identify(from.ip()).await {
        Caller::Unknown => Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "the hub could not ask Tailscale who you are. Try again in a minute.".into(),
        )),
        Caller::Peer(guest) if !guest.owner && !app.guests.is_approved(&guest.node) => {
            Err(ApiError(StatusCode::FORBIDDEN, app.waiting_for_owner()))
        }
        caller => Ok(caller),
    }
}

fn unless_already_seen(asked: &HeaderMap, body: &Value) -> Response {
    let text = body.to_string();
    let tag = format!("W/\"{}\"", URL_SAFE_NO_PAD.encode(&Sha256::digest(&text)[..12]));
    let named = [(header::ETAG, tag.clone()), (header::CACHE_CONTROL, "no-store".into())];
    if asked
        .get(header::IF_NONE_MATCH)
        .is_some_and(|seen| seen.as_bytes() == tag.as_bytes())
    {
        return (StatusCode::NOT_MODIFIED, named).into_response();
    }
    (named, [(header::CONTENT_TYPE, "application/json")], text).into_response()
}

pub async fn data(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let name = hub_name(&app, host);
    let caller = admitted(&app, from).await?;
    let owner = caller.is_owner();
    let mut accounts: Vec<_> = app
        .store
        .all()
        .iter()
        .filter(|e| e.credentials().generation > 0)
        .map(|e| account_view(&app, e))
        .collect();
    if owner {
        for account in &mut accounts {
            account.cache_hit = app.requests.cache_hit(&account.id);
        }
    }
    let mut machines = app.fleet();
    let mut state = json!(app.hub_state.get());
    if !owner {
        state = json!({ "state": state["state"] });
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
    let core = json!({
        "hub": { "name": name, "port": app.cfg.port, "version": update::CURRENT, "state": state },
        "guest": !owner,
        "latest_version": app.updater.latest(),
        "accounts": accounts,
        "machines": machines,
        "usage_written_at": owner.then(|| app.requests.ledger().written_at()).flatten(),
        "add_machine": owner.then(|| add_machine(&name)),
    });
    Ok(unless_already_seen(&headers, &core))
}

pub async fn history(
    State(app): State<Arc<App>>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    admitted(&app, from).await?;
    let listed = |id: &String| app.store.get(id).is_some_and(|e| e.credentials().generation > 0);
    let history: BTreeMap<_, _> = app.usage.history().into_iter().filter(|(id, _)| listed(id)).collect();
    Ok(unless_already_seen(&headers, &json!({ "usage_history": history })))
}

pub async fn requests(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let log = json!({ "requests": app.requests.recent(), "requests_hour": app.requests.last_hour() });
    unless_already_seen(&headers, &log)
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

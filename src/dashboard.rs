use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Value, json};

use crate::api::account_view;
use crate::app::App;
use crate::config::Config;

const PAGE: &str = include_str!("dashboard.html");

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

pub fn add_machine(hub: &str, port: u16) -> Value {
    let command = format!("curl -fsSL http://{hub}:{port}/conflux/install.sh | sh");
    let agent_prompt = format!(
        "Set up conflux on this machine so its Claude Code and Codex use my hub. Run `{command}`. \
         The machine must be on my tailnet. When it finishes, run `conflux status` and tell me whether it can reach the hub."
    );
    let windows_command = format!("irm http://{hub}:{port}/conflux/install.ps1 | iex");
    json!({ "command": command, "windows_command": windows_command, "agent_prompt": agent_prompt })
}

pub async fn data(State(app): State<Arc<App>>, headers: HeaderMap) -> Json<Value> {
    let sync = Config::load(&app.paths).map(|c| c.transcript_sync).unwrap_or(false);
    app.machines.seen(&app.hostname, env!("CARGO_PKG_VERSION"), "hub", sync);
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let name = hub_name(&app, host);
    let accounts: Vec<_> = app
        .store
        .all()
        .iter()
        .filter(|e| e.credentials().generation > 0)
        .map(|e| account_view(&app, e))
        .collect();
    Json(json!({
        "hub": { "name": name, "port": app.cfg.port, "version": env!("CARGO_PKG_VERSION"), "state": app.hub_state.get() },
        "accounts": accounts,
        "machines": app.machines.list(),
        "requests": app.requests.recent(),
        "add_machine": add_machine(&name, app.cfg.port),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_machine_command_uses_the_hub_name() {
        let v = add_machine("hub-box", 8317);
        assert_eq!(v["command"], "curl -fsSL http://hub-box:8317/conflux/install.sh | sh");
        assert!(
            v["agent_prompt"]
                .as_str()
                .unwrap()
                .contains("curl -fsSL http://hub-box:8317")
        );
    }

    #[test]
    fn page_has_no_external_assets() {
        assert!(!PAGE.contains("https://"));
        assert!(PAGE.contains("/conflux/api/dashboard"));
    }
}

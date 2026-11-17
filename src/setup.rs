use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::fsutil;

pub const CLAUDE_TOKEN: &str = "conflux";
pub const CLAUDE_CACHE_TTL_KEY: &str = "CLAUDE_CODE_PROMPT_CACHE_TTL";
pub const CLAUDE_CACHE_TTL: &str = "1h";
pub const CODEX_PROVIDER: &str = "conflux";
const GROK_PLACEHOLDER: &str = "conflux";
const GROK_AUTH_COMMAND: &str = "echo conflux";
const GROK_AUTH_TTL: i64 = 365 * 24 * 3600;

#[derive(Clone, Debug)]
pub struct ClientPaths {
    pub claude_settings: PathBuf,
    pub codex_config: PathBuf,
    pub grok_config: PathBuf,
    pub muse_settings: PathBuf,
}

impl ClientPaths {
    pub fn from_env() -> Result<Self> {
        let home = crate::config::home_dir()?;
        let claude_dir = std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".claude"));
        let codex_dir = std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".codex"));
        let grok_dir = std::env::var("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".grok"));
        let muse_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"))
            .join("muse");
        Ok(Self {
            claude_settings: claude_dir.join("settings.json"),
            codex_config: codex_dir.join("config.toml"),
            grok_config: grok_dir.join("config.toml"),
            muse_settings: muse_dir.join("settings.json"),
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Joined {
    pub active: bool,
    pub claude_base_url: Option<Value>,
    pub claude_auth_token: Option<Value>,
    pub claude_cache_ttl: Option<Value>,
    pub codex_model_provider: Option<String>,
    pub codex_table: Option<String>,
    #[serde(default)]
    pub grok_base_url: Option<String>,
    #[serde(default)]
    pub grok_auth_command: Option<String>,
    #[serde(default)]
    pub grok_auth_ttl: Option<i64>,
    #[serde(default)]
    pub grok_recorded: bool,
    #[serde(default)]
    pub muse_transport: Option<Value>,
    #[serde(default)]
    pub muse_recorded: bool,
    #[serde(default)]
    pub recorded: bool,
}

pub fn joined_path(config_dir: &Path) -> PathBuf {
    config_dir.join("joined.json")
}

pub fn load_joined(config_dir: &Path) -> Result<Option<Joined>> {
    fsutil::read_json(&joined_path(config_dir))
}

pub fn routing(config_dir: &Path) -> bool {
    load_joined(config_dir).ok().flatten().is_some_and(|j| j.active)
}

pub fn set_routing(paths: &ClientPaths, config_dir: &Path, port: u16, on: bool) -> Result<()> {
    if on {
        apply(paths, config_dir, port)?;
    } else {
        restore(paths, config_dir, port)?;
    }
    Ok(())
}

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn read_claude(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(Map::new()),
        Ok(s) => match serde_json::from_str::<Value>(&s).with_context(|| format!("parse {}", path.display()))? {
            Value::Object(m) => Ok(m),
            _ => anyhow::bail!("{} is not a JSON object", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e.into()),
    }
}

fn write_claude(path: &Path, m: Map<String, Value>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(m))?;
    text.push('\n');
    fsutil::write_durable(path, text.as_bytes())
}

fn read_codex(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .parse::<DocumentMut>()
            .with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(e.into()),
    }
}

fn write_codex(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    fsutil::write_durable(path, doc.to_string().as_bytes())
}

fn grok_url(port: u16) -> String {
    format!("{}{}", base_url(port), crate::proxy::GROK_PREFIX.trim_end_matches('/'))
}

fn grok_installed(paths: &ClientPaths) -> bool {
    paths.grok_config.parent().is_some_and(Path::is_dir)
}

fn grok_has_login(paths: &ClientPaths) -> bool {
    let Some(auth) = paths.grok_config.parent().map(|d| d.join("auth.json")) else {
        return false;
    };
    if std::fs::read_to_string(&auth).is_ok_and(|s| s.trim().is_empty()) {
        return false;
    }
    match fsutil::read_json::<Value>(&auth) {
        Ok(None) => false,
        Ok(Some(Value::Object(entries))) => entries
            .values()
            .any(|e| e["auth_mode"].as_str() != Some("external") || e["key"].as_str() != Some(GROK_PLACEHOLDER)),
        _ => true,
    }
}

fn read_grok(path: &Path) -> Result<DocumentMut> {
    let doc = read_codex(path)?;
    for table in ["endpoints", "auth"] {
        if doc.get(table).is_some_and(|t| t.as_table_like().is_none()) {
            anyhow::bail!("`{table}` in {} is not a table", path.display());
        }
    }
    let item = |table: &str, key: &str| doc.get(table).and_then(|t| t.get(key));
    let wrong = [
        (
            "endpoints",
            "cli_chat_proxy_base_url",
            item("endpoints", "cli_chat_proxy_base_url").is_some_and(|v| !v.is_str()),
        ),
        (
            "auth",
            "auth_provider_command",
            item("auth", "auth_provider_command").is_some_and(|v| !v.is_str()),
        ),
        (
            "auth",
            "auth_token_ttl",
            item("auth", "auth_token_ttl").is_some_and(|v| !v.is_integer()),
        ),
    ];
    if let Some((table, key, _)) = wrong.into_iter().find(|w| w.2) {
        anyhow::bail!("`{table}.{key}` in {} has the wrong type", path.display());
    }
    Ok(doc)
}

fn toml_str(doc: &DocumentMut, table: &str, key: &str) -> Option<String> {
    doc.get(table)?.get(key)?.as_str().map(str::to_string)
}

fn toml_int(doc: &DocumentMut, table: &str, key: &str) -> Option<i64> {
    doc.get(table)?.get(key)?.as_integer()
}

fn set_in(doc: &mut DocumentMut, table: &str, key: &str, v: Item) {
    if doc.get(table).and_then(Item::as_table_like).is_none() {
        doc[table] = Item::Table(Table::new());
    }
    if let Some(t) = doc[table].as_table_like_mut() {
        t.insert(key, v);
    }
}

fn restore_in(doc: &mut DocumentMut, table: &str, key: &str, ours: &Item, previous: Option<Item>) {
    let Some(t) = doc.get_mut(table).and_then(Item::as_table_like_mut) else {
        return;
    };
    let same = match (t.get(key).and_then(Item::as_value), ours.as_value()) {
        (Some(a), Some(b)) => a.to_string().trim() == b.to_string().trim(),
        _ => false,
    };
    if !same {
        return;
    }
    match previous {
        Some(p) => {
            t.insert(key, p);
        }
        None => {
            t.remove(key);
        }
    }
    if t.is_empty() {
        doc.remove(table);
    }
}

fn record_grok(joined: &mut Joined, doc: &DocumentMut) {
    joined.grok_base_url = toml_str(doc, "endpoints", "cli_chat_proxy_base_url");
    joined.grok_auth_command = toml_str(doc, "auth", "auth_provider_command");
    joined.grok_auth_ttl = toml_int(doc, "auth", "auth_token_ttl");
    joined.grok_recorded = true;
}

fn apply_grok(paths: &ClientPaths, joined: &Joined, mut doc: DocumentMut, port: u16) -> Result<()> {
    set_in(&mut doc, "endpoints", "cli_chat_proxy_base_url", value(grok_url(port)));
    if grok_has_login(paths) {
        restore_grok_auth(&mut doc, joined);
    } else {
        set_in(&mut doc, "auth", "auth_provider_command", value(GROK_AUTH_COMMAND));
        set_in(&mut doc, "auth", "auth_token_ttl", value(GROK_AUTH_TTL));
    }
    write_codex(&paths.grok_config, &doc)
}

fn restore_grok_auth(doc: &mut DocumentMut, joined: &Joined) {
    if toml_str(doc, "auth", "auth_provider_command").as_deref() != Some(GROK_AUTH_COMMAND) {
        return;
    }
    restore_in(
        doc,
        "auth",
        "auth_provider_command",
        &value(GROK_AUTH_COMMAND),
        joined.grok_auth_command.as_deref().map(value),
    );
    restore_in(
        doc,
        "auth",
        "auth_token_ttl",
        &value(GROK_AUTH_TTL),
        joined.grok_auth_ttl.map(value),
    );
}

fn restore_grok(paths: &ClientPaths, joined: &Joined, mut doc: DocumentMut, port: u16) -> Result<()> {
    restore_in(
        &mut doc,
        "endpoints",
        "cli_chat_proxy_base_url",
        &value(grok_url(port)),
        joined.grok_base_url.as_deref().map(value),
    );
    restore_grok_auth(&mut doc, joined);
    write_codex(&paths.grok_config, &doc)
}

fn muse_transport(port: u16) -> Value {
    serde_json::json!({
        "base_url": format!("{}{}v1", base_url(port), crate::proxy::MUSE_PREFIX),
        "auth": "none",
    })
}

fn apply_muse(paths: &ClientPaths, mut settings: Map<String, Value>, port: u16) -> Result<()> {
    settings.entry("schema_version").or_insert(Value::from(1));
    settings.insert("endpoint_transport".into(), muse_transport(port));
    write_claude(&paths.muse_settings, settings)
}

fn restore_muse(path: &Path, joined: &Joined, mut settings: Map<String, Value>, port: u16) -> Result<()> {
    restore_value(
        &mut settings,
        "endpoint_transport",
        &muse_transport(port),
        &joined.muse_transport,
    );
    write_claude(path, settings)
}

fn conflux_table(port: u16) -> Table {
    let mut t = Table::new();
    t["name"] = value("conflux");
    t["base_url"] = value(format!("{}/v1", base_url(port)));
    t["wire_api"] = value("responses");
    t["requires_openai_auth"] = value(false);
    t["supports_websockets"] = value(true);
    t
}

fn ours_claude(port: u16) -> (Value, Value) {
    (Value::String(base_url(port)), Value::String(CLAUDE_TOKEN.into()))
}

pub fn apply(paths: &ClientPaths, config_dir: &Path, port: u16) -> Result<Joined> {
    let mut joined = load_joined(config_dir)?.unwrap_or_default();
    let (url, token) = ours_claude(port);

    let mut claude = read_claude(&paths.claude_settings)?;
    let env = claude.entry("env").or_insert_with(|| Value::Object(Map::new()));
    let env = env.as_object_mut().context("settings.json `env` is not an object")?;

    let mut doc = read_codex(&paths.codex_config)?;
    let providers_has_ours = doc.get("model_providers").and_then(|p| p.get(CODEX_PROVIDER)).is_some();
    let grok = grok_installed(paths)
        .then(|| read_grok(&paths.grok_config))
        .transpose()?;

    let muse = paths
        .muse_settings
        .parent()
        .is_some_and(Path::is_dir)
        .then(|| read_claude(&paths.muse_settings))
        .transpose()?;

    let mut newly_recorded = false;
    if let Some(g) = grok.as_ref().filter(|_| !joined.grok_recorded) {
        record_grok(&mut joined, g);
        newly_recorded = true;
    }
    if let Some(m) = muse.as_ref().filter(|_| !joined.muse_recorded) {
        joined.muse_transport = m.get("endpoint_transport").cloned();
        joined.muse_recorded = true;
        newly_recorded = true;
    }
    if !joined.recorded {
        joined.claude_base_url = env.get("ANTHROPIC_BASE_URL").cloned();
        joined.claude_auth_token = env.get("ANTHROPIC_AUTH_TOKEN").cloned();
        joined.claude_cache_ttl = env.get(CLAUDE_CACHE_TTL_KEY).cloned();
        joined.codex_model_provider = doc.get("model_provider").and_then(|v| v.as_str()).map(str::to_string);
        joined.codex_table = providers_has_ours
            .then(|| doc["model_providers"][CODEX_PROVIDER].as_table().map(|t| t.to_string()))
            .flatten();
        joined.recorded = true;
        joined.active = false;
        newly_recorded = true;
    }
    if newly_recorded {
        fsutil::ensure_private_dir(config_dir)?;
        fsutil::write_json_durable(&joined_path(config_dir), &joined)?;
    }

    env.insert("ANTHROPIC_BASE_URL".into(), url);
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), token);
    env.entry(CLAUDE_CACHE_TTL_KEY)
        .or_insert_with(|| Value::String(CLAUDE_CACHE_TTL.into()));
    write_claude(&paths.claude_settings, claude)?;

    doc["model_provider"] = value(CODEX_PROVIDER);
    if doc.get("model_providers").is_none() {
        let mut t = Table::new();
        t.set_implicit(true);
        doc["model_providers"] = Item::Table(t);
    }
    doc["model_providers"][CODEX_PROVIDER] = Item::Table(conflux_table(port));
    write_codex(&paths.codex_config, &doc)?;
    if let Some(g) = grok {
        apply_grok(paths, &joined, g, port)?;
    }
    if let Some(m) = muse {
        apply_muse(paths, m, port)?;
    }

    joined.active = true;
    fsutil::ensure_private_dir(config_dir)?;
    fsutil::write_json_durable(&joined_path(config_dir), &joined)?;
    Ok(joined)
}

fn restore_value(env: &mut Map<String, Value>, key: &str, ours: &Value, previous: &Option<Value>) {
    if env.get(key) != Some(ours) {
        return;
    }
    match previous {
        Some(v) => {
            env.insert(key.into(), v.clone());
        }
        None => {
            env.remove(key);
        }
    }
}

pub fn restore(paths: &ClientPaths, config_dir: &Path, port: u16) -> Result<Option<Joined>> {
    let Some(mut joined) = load_joined(config_dir)? else {
        return Ok(None);
    };
    if !joined.recorded {
        return Ok(Some(joined));
    }
    let (url, token) = ours_claude(port);
    let grok = (joined.grok_recorded && paths.grok_config.exists())
        .then(|| read_grok(&paths.grok_config))
        .transpose()?;
    let muse = (joined.muse_recorded && paths.muse_settings.exists())
        .then(|| read_claude(&paths.muse_settings))
        .transpose()?;

    let mut claude = read_claude(&paths.claude_settings)?;
    if let Some(env) = claude.get_mut("env").and_then(Value::as_object_mut) {
        restore_value(env, "ANTHROPIC_BASE_URL", &url, &joined.claude_base_url);
        restore_value(env, "ANTHROPIC_AUTH_TOKEN", &token, &joined.claude_auth_token);
        restore_value(
            env,
            CLAUDE_CACHE_TTL_KEY,
            &Value::String(CLAUDE_CACHE_TTL.into()),
            &joined.claude_cache_ttl,
        );
        if env.is_empty() {
            claude.remove("env");
        }
    }
    write_claude(&paths.claude_settings, claude)?;

    let mut doc = read_codex(&paths.codex_config)?;
    if doc.get("model_provider").and_then(|v| v.as_str()) == Some(CODEX_PROVIDER) {
        match &joined.codex_model_provider {
            Some(p) => doc["model_provider"] = value(p.as_str()),
            None => {
                doc.remove("model_provider");
            }
        }
    }
    if let Some(providers) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
        let ours = conflux_table(port).to_string();
        let current = providers
            .get(CODEX_PROVIDER)
            .and_then(Item::as_table)
            .map(|t| t.to_string());
        if current.as_deref() == Some(ours.as_str()) {
            match &joined.codex_table {
                Some(prev) => {
                    if let Ok(prev_doc) = format!("[t]\n{prev}").parse::<DocumentMut>()
                        && let Some(t) = prev_doc.get("t").cloned()
                    {
                        providers.insert(CODEX_PROVIDER, t);
                    }
                }
                None => {
                    providers.remove(CODEX_PROVIDER);
                }
            }
        }
        if providers.is_empty() {
            doc.remove("model_providers");
        }
    }
    write_codex(&paths.codex_config, &doc)?;
    if let Some(g) = grok {
        restore_grok(paths, &joined, g, port)?;
    }
    if let Some(m) = muse {
        restore_muse(&paths.muse_settings, &joined, m, port)?;
    }

    joined.active = false;
    joined.recorded = false;
    joined.grok_recorded = false;
    joined.muse_recorded = false;
    fsutil::write_json_durable(&joined_path(config_dir), &joined)?;
    Ok(Some(joined))
}

pub fn env_warnings() -> Vec<String> {
    [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "OPENAI_BASE_URL",
        "GROK_HOME",
        "GROK_CLI_CHAT_PROXY_BASE_URL",
    ]
    .iter()
    .filter(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()))
    .map(|k| format!("{k} is set in your shell. It can override or bypass the config conflux wrote."))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> (ClientPaths, PathBuf) {
        (
            ClientPaths {
                claude_settings: dir.join("claude/settings.json"),
                codex_config: dir.join("codex/config.toml"),
                grok_config: dir.join("grok/config.toml"),
                muse_settings: dir.join("muse/settings.json"),
            },
            dir.join("conflux"),
        )
    }

    const CODEX: &str = "model = \"gpt-6.1-sol\"\n\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";

    #[test]
    fn apply_then_restore_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        std::fs::create_dir_all(p.claude_settings.parent().unwrap()).unwrap();
        std::fs::create_dir_all(p.codex_config.parent().unwrap()).unwrap();
        let claude_before = "{\n  \"theme\": \"dark\",\n  \"autoCompactEnabled\": true\n}\n";
        std::fs::write(&p.claude_settings, claude_before).unwrap();
        std::fs::write(&p.codex_config, CODEX).unwrap();
        std::fs::create_dir_all(p.grok_config.parent().unwrap()).unwrap();
        let grok_before = "endpoints = { web_base_url = \"https://example.com\" }\n\n[ui]\nyolo = false\n";
        std::fs::write(&p.grok_config, grok_before).unwrap();
        std::fs::create_dir_all(p.muse_settings.parent().unwrap()).unwrap();
        let muse_before = "{\n  \"schema_version\": 1,\n  \"model\": \"muse-wonder-full-1.0\"\n}\n";
        std::fs::write(&p.muse_settings, muse_before).unwrap();

        apply(&p, &conf, 8317).unwrap();
        let claude: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        assert_eq!(claude["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8317");
        assert_eq!(claude["env"][CLAUDE_CACHE_TTL_KEY], "1h");
        assert_eq!(claude.as_object().unwrap().keys().next().unwrap(), "theme");
        let codex = std::fs::read_to_string(&p.codex_config).unwrap();
        assert!(codex.contains("model_provider = \"conflux\""));
        assert!(codex.contains("[model_providers.conflux]"));
        assert!(codex.contains("supports_websockets = true"));
        assert!(codex.contains("trust_level = \"trusted\""));
        let grok: toml::Value = toml::from_str(&std::fs::read_to_string(&p.grok_config).unwrap()).unwrap();
        assert_eq!(
            grok["endpoints"]["cli_chat_proxy_base_url"].as_str(),
            Some("http://127.0.0.1:8317/grok/v1")
        );
        assert_eq!(grok["endpoints"]["web_base_url"].as_str(), Some("https://example.com"));
        assert_eq!(grok["auth"]["auth_provider_command"].as_str(), Some(GROK_AUTH_COMMAND));
        let muse: Value = serde_json::from_str(&std::fs::read_to_string(&p.muse_settings).unwrap()).unwrap();
        assert_eq!(muse["endpoint_transport"]["base_url"], "http://127.0.0.1:8317/muse/v1");
        assert_eq!(muse["endpoint_transport"]["auth"], "none");

        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        assert_eq!(std::fs::read_to_string(&p.claude_settings).unwrap(), claude_before);
        assert_eq!(std::fs::read_to_string(&p.codex_config).unwrap(), CODEX);
        assert_eq!(std::fs::read_to_string(&p.grok_config).unwrap(), grok_before);
        assert_eq!(std::fs::read_to_string(&p.muse_settings).unwrap(), muse_before);
    }

    #[test]
    fn a_grok_login_keeps_its_own_auth() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        let grok_dir = p.grok_config.parent().unwrap();
        std::fs::create_dir_all(grok_dir).unwrap();
        let read = || -> toml::Value { toml::from_str(&std::fs::read_to_string(&p.grok_config).unwrap()).unwrap() };
        std::fs::write(grok_dir.join("auth.json"), "").unwrap();
        apply(&p, &conf, 8317).unwrap();
        assert_eq!(
            read()["auth"]["auth_provider_command"].as_str(),
            Some(GROK_AUTH_COMMAND)
        );

        std::fs::write(
            grok_dir.join("auth.json"),
            r#"{"https://auth.x.ai::client":{"auth_mode":"external","key":"conflux"}}"#,
        )
        .unwrap();
        apply(&p, &conf, 8317).unwrap();
        assert_eq!(
            read()["auth"]["auth_provider_command"].as_str(),
            Some(GROK_AUTH_COMMAND)
        );
        for login in [
            r#"{"https://auth.x.ai::client":{"auth_mode":"external","key":"sso-token"}}"#,
            r#"{"https://auth.x.ai::client":{"auth_mode":"oidc","key":"k"}}"#,
            r#"{"https://auth.x.ai::cli"#,
        ] {
            std::fs::write(grok_dir.join("auth.json"), login).unwrap();
            apply(&p, &conf, 8317).unwrap();
            assert!(read()["endpoints"]["cli_chat_proxy_base_url"].is_str());
            assert!(read().get("auth").is_none(), "{login} keeps its own auth");
        }
        restore(&p, &conf, 8317).unwrap();
        assert_eq!(std::fs::read_to_string(&p.grok_config).unwrap().trim(), "");
    }

    #[test]
    fn an_upgraded_join_records_new_tools_before_changing_them() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        apply(&p, &conf, 8317).unwrap();
        let mut old: Value = serde_json::from_str(&std::fs::read_to_string(joined_path(&conf)).unwrap()).unwrap();
        for key in [
            "grok_base_url",
            "grok_auth_command",
            "grok_auth_ttl",
            "grok_recorded",
            "muse_transport",
            "muse_recorded",
        ] {
            old.as_object_mut().unwrap().remove(key);
        }
        std::fs::write(joined_path(&conf), old.to_string()).unwrap();
        std::fs::create_dir_all(p.grok_config.parent().unwrap()).unwrap();
        let mine = "[auth]\nauth_provider_command = \"my-sso\"\n";
        std::fs::write(&p.grok_config, mine).unwrap();
        std::fs::create_dir_all(p.muse_settings.parent().unwrap()).unwrap();
        let my_muse = "{\n  \"schema_version\": 1,\n  \"endpoint_transport\": {\n    \"base_url\": \"https://gateway.example.com/v1\"\n  }\n}\n";
        std::fs::write(&p.muse_settings, my_muse).unwrap();

        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        assert_eq!(std::fs::read_to_string(&p.grok_config).unwrap(), mine);
        assert_eq!(std::fs::read_to_string(&p.muse_settings).unwrap(), my_muse);
    }

    #[test]
    fn restore_keeps_values_the_user_changed() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        apply(&p, &conf, 8317).unwrap();
        let mut claude: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        claude["env"]["ANTHROPIC_BASE_URL"] = "http://elsewhere".into();
        std::fs::write(&p.claude_settings, serde_json::to_string(&claude).unwrap()).unwrap();
        restore(&p, &conf, 8317).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        assert_eq!(after["env"]["ANTHROPIC_BASE_URL"], "http://elsewhere");
        assert!(after["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    }

    #[test]
    fn restores_previous_values() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        std::fs::create_dir_all(p.claude_settings.parent().unwrap()).unwrap();
        std::fs::create_dir_all(p.codex_config.parent().unwrap()).unwrap();
        std::fs::write(
            &p.claude_settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://old","CLAUDE_CODE_PROMPT_CACHE_TTL":"5m"}}"#,
        )
        .unwrap();
        std::fs::write(&p.codex_config, "model_provider = \"openai\"\n").unwrap();
        apply(&p, &conf, 9000).unwrap();
        let during: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        assert_eq!(during["env"][CLAUDE_CACHE_TTL_KEY], "5m");
        restore(&p, &conf, 9000).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        assert_eq!(after["env"]["ANTHROPIC_BASE_URL"], "http://old");
        assert_eq!(after["env"][CLAUDE_CACHE_TTL_KEY], "5m");
        assert_eq!(
            std::fs::read_to_string(&p.codex_config).unwrap(),
            "model_provider = \"openai\"\n"
        );
    }

    #[test]
    fn a_failed_join_keeps_the_original_values() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        std::fs::create_dir_all(p.claude_settings.parent().unwrap()).unwrap();
        std::fs::write(&p.claude_settings, r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"mine"}}"#).unwrap();
        std::fs::create_dir_all(&p.codex_config).unwrap();
        assert!(apply(&p, &conf, 8317).is_err());
        std::fs::remove_dir(&p.codex_config).unwrap();
        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        assert_eq!(after["env"]["ANTHROPIC_AUTH_TOKEN"], "mine");
    }

    #[test]
    fn a_broken_grok_config_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        std::fs::create_dir_all(p.grok_config.parent().unwrap()).unwrap();
        for broken in [
            "not = [toml",
            "auth = \"x\"\n",
            "[endpoints]\ncli_chat_proxy_base_url = 1\n",
        ] {
            std::fs::write(&p.grok_config, broken).unwrap();
            assert!(apply(&p, &conf, 8317).is_err(), "{broken}");
            assert!(!p.claude_settings.exists());
        }

        std::fs::write(&p.grok_config, "").unwrap();
        apply(&p, &conf, 8317).unwrap();
        let routed = std::fs::read_to_string(&p.claude_settings).unwrap();
        std::fs::write(&p.grok_config, "not = [toml").unwrap();
        assert!(restore(&p, &conf, 8317).is_err());
        assert_eq!(std::fs::read_to_string(&p.claude_settings).unwrap(), routed);
        assert!(routing(&conf));
    }

    #[test]
    fn each_on_records_what_was_there_at_that_moment() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        std::fs::write(&p.claude_settings, r#"{"env":{"ANTHROPIC_BASE_URL":"http://new"}}"#).unwrap();
        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&p.claude_settings).unwrap()).unwrap();
        assert_eq!(after["env"]["ANTHROPIC_BASE_URL"], "http://new");
    }

    #[test]
    fn missing_files_are_created_and_removed_cleanly() {
        let d = tempfile::tempdir().unwrap();
        let (p, conf) = paths(d.path());
        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        assert_eq!(std::fs::read_to_string(&p.claude_settings).unwrap().trim(), "{}");
        assert_eq!(std::fs::read_to_string(&p.codex_config).unwrap().trim(), "");
    }
}

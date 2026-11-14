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

#[derive(Clone, Debug)]
pub struct ClientPaths {
    pub claude_settings: PathBuf,
    pub codex_config: PathBuf,
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
        Ok(Self {
            claude_settings: claude_dir.join("settings.json"),
            codex_config: codex_dir.join("config.toml"),
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

    joined.active = false;
    joined.recorded = false;
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

        apply(&p, &conf, 8317).unwrap();
        restore(&p, &conf, 8317).unwrap();
        assert_eq!(std::fs::read_to_string(&p.claude_settings).unwrap(), claude_before);
        assert_eq!(std::fs::read_to_string(&p.codex_config).unwrap(), CODEX);
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

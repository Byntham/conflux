use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fsutil;

pub const DEFAULT_PORT: u16 = 8317;

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    under_test: bool,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        if let Ok(home) = std::env::var("CONFLUX_HOME") {
            let base = PathBuf::from(home);
            return Ok(Self {
                config_dir: base.join("config"),
                data_dir: base.join("data"),
                under_test: false,
            });
        }
        let home = home_dir()?;
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".config"))
            .join("conflux");
        let data_dir = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local/share"))
            .join("conflux");
        Ok(Self {
            config_dir,
            data_dir,
            under_test: false,
        })
    }

    pub fn under(base: &std::path::Path) -> Self {
        Self {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            under_test: true,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn accounts_dir(&self) -> PathBuf {
        self.data_dir.join("accounts")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.data_dir.join("hub.lock")
    }
    pub fn transcripts_dir(&self) -> Result<PathBuf> {
        if std::env::var_os("CONFLUX_HOME").is_some() || self.under_test {
            return Ok(self.data_dir.join("transcripts"));
        }
        Ok(home_dir()?.join("conflux").join("transcripts"))
    }
    pub fn management_key_file(&self) -> PathBuf {
        self.config_dir.join("management-key")
    }
    pub fn has_accounts(&self) -> bool {
        std::fs::read_dir(self.accounts_dir()).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.ends_with(".json") && !n.ends_with(".settings.json")
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Hub,
    Node,
    Waiting,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub hub: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub claude_code_only: bool,
    #[serde(default)]
    pub transcript_sync: bool,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            role: Role::Hub,
            hub: None,
            port: DEFAULT_PORT,
            claude_code_only: true,
            transcript_sync: false,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        match std::fs::read_to_string(paths.config_file()) {
            Ok(s) => Ok(toml::from_str(&s).context("parse config.toml")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fsutil::ensure_private_dir(&paths.config_dir)?;
        fsutil::write_durable(&paths.config_file(), toml::to_string_pretty(self)?.as_bytes())
    }
}

#[derive(Clone, Debug)]
pub struct Endpoints {
    pub anthropic_api: String,
    pub claude_authorize: String,
    pub claude_token: String,
    pub codex_api: String,
    pub chatgpt_backend: String,
    pub openai_auth: String,
    pub releases: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            anthropic_api: "https://api.anthropic.com".into(),
            claude_authorize: "https://claude.ai/oauth/authorize".into(),
            claude_token: "https://platform.claude.com/v1/oauth/token".into(),
            codex_api: "https://chatgpt.com/backend-api/codex".into(),
            chatgpt_backend: "https://chatgpt.com/backend-api".into(),
            openai_auth: "https://auth.openai.com".into(),
            releases: "https://github.com/Byntham/conflux/releases".into(),
        }
    }
}

impl Endpoints {
    pub fn from_env() -> Self {
        let mut e = Self::default();
        let set = |field: &mut String, var: &str| {
            if let Ok(v) = std::env::var(var) {
                *field = v.trim_end_matches('/').to_string();
            }
        };
        set(&mut e.anthropic_api, "CONFLUX_ANTHROPIC_API");
        set(&mut e.claude_authorize, "CONFLUX_CLAUDE_AUTHORIZE");
        set(&mut e.claude_token, "CONFLUX_CLAUDE_TOKEN");
        set(&mut e.codex_api, "CONFLUX_CODEX_API");
        set(&mut e.chatgpt_backend, "CONFLUX_CHATGPT_BACKEND");
        set(&mut e.openai_auth, "CONFLUX_OPENAI_AUTH");
        set(&mut e.releases, "CONFLUX_RELEASES");
        e
    }

    pub fn all_at(base: &str) -> Self {
        let b = base.trim_end_matches('/');
        Self {
            anthropic_api: b.to_string(),
            claude_authorize: format!("{b}/oauth/authorize"),
            claude_token: format!("{b}/v1/oauth/token"),
            codex_api: format!("{b}/backend-api/codex"),
            chatgpt_backend: format!("{b}/backend-api"),
            openai_auth: b.to_string(),
            releases: format!("{b}/releases"),
        }
    }
}

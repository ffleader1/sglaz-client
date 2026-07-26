use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One app this agent syncs, as told by the server. binding_id is the server's
/// id; file_path is where this app's .env lives on this machine.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Binding {
    pub binding_id: String,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub file_path: String,
}

/// One deployment (runnable service) this agent supervises. Persisted so the
/// agent can report each service's state — and the version it last deployed —
/// on every poll, even straight after a restart.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DeployRecord {
    pub deployment_id: String,
    pub unit_name: String,
    #[serde(default)]
    pub version: String,
}

/// Persistent client configuration. One identity (token) per machine; the
/// server drives the list of app bindings.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Config {
    pub server_url: String,
    pub token: String,
    pub client_id: String,
    #[serde(default = "default_interval")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    #[serde(default)]
    pub deployments: Vec<DeployRecord>,
}

fn default_interval() -> u64 {
    15
}

/// Where config.yaml lives. Override with SGLAZ_CONFIG. Otherwise the standard
/// per-OS config dir (Linux: ~/.config/sglaz, macOS: ~/Library/Application
/// Support/sglaz, Windows: %APPDATA%\sglaz\config).
pub fn config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SGLAZ_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let pd = directories::ProjectDirs::from("", "", "sglaz")
        .context("could not determine a config directory for this OS")?;
    Ok(pd.config_dir().join("config.yaml"))
}

impl Config {
    /// Load config, erroring if it's missing (used by the daemon).
    pub fn load() -> Result<Config> {
        let path = config_path()?;
        let data = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no config at {} — run `sglaz -connect <code>` first",
                path.display()
            )
        })?;
        serde_yaml::from_str(&data).context("config.yaml is malformed")
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let data = serde_yaml::to_string(self)?;
        std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

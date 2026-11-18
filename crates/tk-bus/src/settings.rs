//! Basic settings (spec §6.1): agents, workspaces, appearance, limits. Stored in
//! `<home>/settings.toml`; a default file is written on first use.

use serde::{Deserialize, Serialize};

use crate::model::AgentConfig;
use crate::paths;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default = "default_agents")]
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub workspaces: Vec<String>,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            agents: default_agents(),
            workspaces: vec![],
            font_family: default_font_family(),
            font_size: default_font_size(),
            max_sessions: default_max_sessions(),
        }
    }
}

fn default_agents() -> Vec<AgentConfig> {
    vec![
        AgentConfig {
            id: "claude".into(),
            name: "Claude Code".into(),
            command: "claude".into(),
            args: vec![],
            mcp: true,
        },
        AgentConfig {
            id: "shell".into(),
            name: "PowerShell".into(),
            command: if cfg!(windows) { "powershell.exe".into() } else { "bash".into() },
            args: if cfg!(windows) { vec!["-NoLogo".into()] } else { vec![] },
            mcp: false,
        },
    ]
}
fn default_font_family() -> String {
    if cfg!(windows) { "Consolas".into() } else { "Menlo".into() }
}
fn default_font_size() -> f32 {
    14.0
}
fn default_max_sessions() -> usize {
    16
}

impl Settings {
    pub fn path() -> std::path::PathBuf {
        paths::home().join("settings.toml")
    }

    pub fn load() -> Settings {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                tracing::error!("settings.toml invalid, using defaults: {e}");
                Settings::default()
            }),
            Err(_) => {
                let s = Settings::default();
                if let Ok(text) = toml::to_string_pretty(&s) {
                    let _ = std::fs::create_dir_all(paths::home());
                    let _ = std::fs::write(&path, text);
                }
                s
            }
        }
    }
}

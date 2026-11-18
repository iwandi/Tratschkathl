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
    /// Built-in agents already offered once. New built-ins are added to existing settings
    /// exactly once, so an agent the user removed stays removed.
    #[serde(default)]
    pub offered_agents: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            agents: default_agents(),
            workspaces: vec![],
            font_family: default_font_family(),
            font_size: default_font_size(),
            max_sessions: default_max_sessions(),
            offered_agents: default_agents().into_iter().map(|a| a.id).collect(),
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
            kind: "terminal".into(),
        },
        AgentConfig {
            id: "claude-chat".into(),
            name: "Claude (chat)".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@agentclientprotocol/claude-agent-acp".into()],
            mcp: true,
            kind: "acp".into(),
        },
        AgentConfig {
            id: "shell".into(),
            name: "PowerShell".into(),
            command: if cfg!(windows) { "powershell.exe".into() } else { "bash".into() },
            args: if cfg!(windows) { vec!["-NoLogo".into()] } else { vec![] },
            mcp: false,
            kind: "terminal".into(),
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
    /// Add built-in agents that were never offered before. Returns true if anything changed.
    fn offer_new_defaults(&mut self) -> bool {
        let mut changed = false;
        for a in default_agents() {
            if self.offered_agents.contains(&a.id) {
                continue;
            }
            self.offered_agents.push(a.id.clone());
            if !self.agents.iter().any(|x| x.id == a.id) {
                self.agents.push(a);
            }
            changed = true;
        }
        changed
    }

    pub fn path() -> std::path::PathBuf {
        paths::home().join("settings.toml")
    }

    pub fn load() -> Settings {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Settings>(&s) {
                Ok(mut settings) => {
                    if settings.offer_new_defaults() {
                        if let Ok(text) = toml::to_string_pretty(&settings) {
                            let _ = std::fs::write(&path, text);
                        }
                    }
                    settings
                }
                Err(e) => {
                    tracing::error!("settings.toml invalid, using defaults: {e}");
                    Settings::default()
                }
            },
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

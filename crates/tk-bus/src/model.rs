use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionInfo {
    pub id: String,
    /// `terminal` for M0; `chat` arrives with ACP.
    pub kind: String,
    pub agent: String,
    pub title: String,
    pub cwd: String,
    /// `running` | `ended`
    pub state: String,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Pass the orchestrator tools via `--mcp-config` (Claude Code).
    #[serde(default)]
    pub mcp: bool,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

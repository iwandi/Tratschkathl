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

/// A current fact about a session (spec §4.2). `kind` is set for `Ref` values
/// (e.g. `git.branch`); `None` means plain text.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Fact {
    pub id: i64,
    pub session: String,
    pub key: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `agent` | `user` | `plugin:<id>` | `automation`
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub announced_at: i64,
}

/// A fact to set, as passed to `session.create` or announced by a plugin.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct FactInput {
    pub key: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// One view from `views.toml` (spec §4.3). M1 implements `type = "tree"`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ViewConfig {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default = "default_dock")]
    pub dock: String,
    /// Tree: nesting by fact keys.
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default = "default_true")]
    pub show_unassigned: bool,
    /// Show "+" on nodes to start a session pre-seeded with the node's facts.
    #[serde(default)]
    pub create: bool,
    /// Include ended sessions.
    #[serde(default)]
    pub show_ended: bool,
}

fn default_dock() -> String {
    "left".into()
}
fn default_true() -> bool {
    true
}

/// A node of a rendered tree view.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TreeNode {
    pub key: String,
    pub value: String,
    /// Facts on the path from the root to this node: what "+" pre-seeds.
    pub facts: Vec<FactInput>,
    /// Sessions attached directly to this node (newest first).
    pub sessions: Vec<String>,
    pub children: Vec<TreeNode>,
    /// Distinct sessions in the whole subtree.
    pub count: usize,
    /// Working folder of the newest session in the subtree: default folder for "+".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RenderedView {
    pub view: ViewConfig,
    pub root: TreeNode,
}

/// A document a provider offers for a session's facts (spec §4.4).
/// `uri` is `tk://doc/<kind>/<provider>/<ref>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DocRef {
    pub uri: String,
    pub kind: String,
    pub title: String,
    pub provider: String,
}

/// A document's content. `body` depends on `kind`:
/// - `markdown`: `{ path, text }`
/// - `changeset`: `{ root, branch, files: [{ path, status, added, removed, diff }], truncated }`
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Document {
    pub uri: String,
    pub kind: String,
    pub title: String,
    pub body: serde_json::Value,
}

pub const DOC_PREFIX: &str = "tk://doc/";

pub fn doc_uri(kind: &str, provider: &str, reference: &str) -> String {
    format!("{DOC_PREFIX}{kind}/{provider}/{reference}")
}

/// Split a document URI into (kind, provider, ref).
pub fn parse_doc_uri(uri: &str) -> Option<(&str, &str, &str)> {
    let rest = uri.strip_prefix(DOC_PREFIX)?;
    let mut it = rest.splitn(3, '/');
    Some((it.next()?, it.next()?, it.next()?))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

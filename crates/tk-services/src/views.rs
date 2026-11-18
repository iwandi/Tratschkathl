//! View engine (spec §4.3). Views live in `<home>/views.toml`. M1 renders `tree` views:
//! sessions nested by fact keys. Board and chain views follow in M3.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use tk_bus::model::{Fact, FactInput, RenderedView, SessionInfo, TreeNode, ViewConfig};

const DEFAULT_VIEWS: &str = r#"# Views (spec §4.3). Agents configure these; you can also edit this file by hand.
# It is re-read on every change, no restart needed.

[[view]]
id = "projects"
title = "Projects"
type = "tree"
dock = "left"
levels = ["project", "branch", "issue"]   # nesting by fact keys
show_unassigned = true                     # sessions without a project fact
create = true                              # "+" on a node starts a session with the node's facts
show_ended = false
"#;

#[derive(Serialize, Deserialize, Default)]
struct ViewFile {
    #[serde(default, rename = "view")]
    views: Vec<ViewConfig>,
}

/// `views.toml`, agent-editable with history (`view_config_get` / `view_config_set`).
pub const FILE: crate::versioned::VersionedFile = crate::versioned::VersionedFile { name: "views", default_text: DEFAULT_VIEWS };

pub fn path() -> std::path::PathBuf {
    FILE.path()
}

/// Load the view config, writing the default on first use.
pub fn load() -> Result<Vec<ViewConfig>, String> {
    parse(&FILE.text())
}

pub fn parse(text: &str) -> Result<Vec<ViewConfig>, String> {
    let file: ViewFile = toml::from_str(text).map_err(|e| format!("views.toml: {e}"))?;
    let mut ids = HashSet::new();
    for v in &file.views {
        if !ids.insert(v.id.clone()) {
            return Err(format!("views.toml: duplicate view id '{}'", v.id));
        }
        if v.kind == "tree" && v.levels.is_empty() {
            return Err(format!("views.toml: tree view '{}' needs levels", v.id));
        }
        for k in &v.levels {
            crate::facts::validate_key(k)?;
        }
    }
    Ok(file.views)
}

/// Render a tree view over the given sessions and their current facts.
pub fn tree(view: &ViewConfig, sessions: &[SessionInfo], facts: &[Fact]) -> TreeNode {
    let mut root = TreeNode::default();
    let mut unassigned = TreeNode { key: String::new(), value: "(no facts)".into(), ..Default::default() };

    for s in sessions.iter().filter(|s| view.show_ended || s.state == "running") {
        // Every combination of values along the levels (multi-valued keys fan out).
        let mut paths: Vec<Vec<FactInput>> = vec![vec![]];
        for level in &view.levels {
            let values: Vec<&Fact> = facts.iter().filter(|f| f.session == s.id && &f.key == level).collect();
            if values.is_empty() {
                break;
            }
            paths = paths
                .into_iter()
                .flat_map(|p| {
                    values.iter().map(move |f| {
                        let mut p = p.clone();
                        p.push(FactInput { key: f.key.clone(), value: f.value.clone(), kind: f.kind.clone() });
                        p
                    })
                })
                .collect();
        }
        for path in paths {
            if path.is_empty() {
                if view.show_unassigned {
                    unassigned.sessions.push(s.id.clone());
                }
                continue;
            }
            let mut node = &mut root;
            for (i, step) in path.iter().enumerate() {
                let ix = match node.children.iter().position(|c| c.key == step.key && c.value == step.value) {
                    Some(ix) => ix,
                    None => {
                        node.children.push(TreeNode {
                            key: step.key.clone(),
                            value: step.value.clone(),
                            facts: path[..=i].to_vec(),
                            ..Default::default()
                        });
                        node.children.len() - 1
                    }
                };
                node = &mut node.children[ix];
            }
            node.sessions.push(s.id.clone());
        }
    }
    if !unassigned.sessions.is_empty() {
        root.children.push(unassigned);
    }
    let cwd_of: BTreeMap<&str, (&str, i64)> =
        sessions.iter().map(|s| (s.id.as_str(), (s.cwd.as_str(), s.created_at))).collect();
    finish(&mut root, &cwd_of);
    root
}

/// Sort children, and fill `count` and `cwd` bottom-up. Returns the subtree's sessions.
fn finish(node: &mut TreeNode, cwd_of: &BTreeMap<&str, (&str, i64)>) -> HashSet<String> {
    node.children.sort_by(|a, b| (a.key.is_empty(), &a.value).cmp(&(b.key.is_empty(), &b.value)));
    let mut all: HashSet<String> = node.sessions.iter().cloned().collect();
    for c in node.children.iter_mut() {
        all.extend(finish(c, cwd_of));
    }
    node.count = all.len();
    node.cwd = all
        .iter()
        .filter_map(|id| cwd_of.get(id.as_str()))
        .max_by_key(|(_, created)| *created)
        .map(|(cwd, _)| cwd.to_string());
    all
}

pub fn render_all(sessions: &[SessionInfo], facts: &[Fact]) -> Result<Vec<RenderedView>, String> {
    Ok(load()?
        .into_iter()
        .filter(|v| v.kind == "tree")
        .map(|v| {
            let root = tree(&v, sessions, facts);
            RenderedView { view: v, root }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, cwd: &str, at: i64) -> SessionInfo {
        SessionInfo { id: id.into(), cwd: cwd.into(), state: "running".into(), created_at: at, ..Default::default() }
    }
    fn f(session: &str, k: &str, v: &str) -> Fact {
        Fact { session: session.into(), key: k.into(), value: v.into(), source: "agent".into(), ..Default::default() }
    }

    #[test]
    fn groups_by_levels_and_fans_out_multi() {
        let view = parse(DEFAULT_VIEWS).unwrap().remove(0);
        let sessions = vec![s("a", "/x", 1), s("b", "/y", 2), s("c", "/z", 3)];
        let facts = vec![
            f("a", "project", "tk"),
            f("a", "branch", "main"),
            f("b", "project", "tk"),
            f("b", "branch", "feat"),
            f("b", "issue", "T-1"),
            f("b", "issue", "T-2"),
        ];
        let root = tree(&view, &sessions, &facts);
        assert_eq!(root.children.len(), 2); // tk + unassigned
        let tk = &root.children[0];
        assert_eq!((tk.value.as_str(), tk.count), ("tk", 2));
        assert_eq!(tk.cwd.as_deref(), Some("/y"));
        let feat = tk.children.iter().find(|c| c.value == "feat").unwrap();
        assert_eq!(feat.children.len(), 2);
        assert_eq!(feat.children[0].facts.len(), 3);
        assert_eq!(root.children[1].sessions, vec!["c"]);
    }
}

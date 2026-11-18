//! View engine (spec §4.3). Views live in `<home>/views.toml`:
//! - `tree`: sessions nested by fact keys
//! - `board`: columns by the value of one fact (kanban), filtered by facts
//! - `chain`: sessions linked by a `continues` fact, in order (continuation bar)
//!
//! Filters and scopes may say `$selected.<key>`: the value of that fact on the session the
//! user has selected (the UI sends its facts as `selection`).

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use tk_bus::model::{
    Board, BoardColumn, Chain, ChainStep, Fact, FactInput, RenderedView, SessionInfo, TreeNode, ViewConfig,
};

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
        let fail = |msg: &str| Err(format!("views.toml: view '{}' {msg}", v.id));
        match v.kind.as_str() {
            "tree" if v.levels.is_empty() => return fail("(tree) needs levels"),
            "board" if v.column_key.is_none() || v.columns.is_empty() => return fail("(board) needs column_key and columns"),
            "chain" if v.link_key.is_none() || v.scope.is_none() => return fail("(chain) needs link_key and scope"),
            "tree" | "board" | "chain" => {}
            other => return fail(&format!("has unknown type '{other}' (tree | board | chain)")),
        }
        if !matches!(v.dock.as_str(), "left" | "center" | "top") {
            return fail("has an unknown dock (left | center | top)");
        }
        let keys = v.levels.iter().chain(v.column_key.iter()).chain(v.link_key.iter()).chain(v.label_key.iter()).chain(v.filter.keys());
        for k in keys {
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

/// Resolve `$selected.<key>` against the selection; plain values pass through.
fn resolve(value: &str, selection: &BTreeMap<String, String>) -> Option<String> {
    match value.strip_prefix("$selected.") {
        Some(key) => selection.get(key).cloned(),
        None => Some(value.to_string()),
    }
}

fn has_fact(facts: &[Fact], session: &str, key: &str, value: &str) -> bool {
    facts.iter().any(|f| f.session == session && f.key == key && f.value == value)
}

fn fact_value<'a>(facts: &'a [Fact], session: &str, key: &str) -> Option<&'a str> {
    facts.iter().find(|f| f.session == session && f.key == key).map(|f| f.value.as_str())
}

pub fn board(view: &ViewConfig, sessions: &[SessionInfo], facts: &[Fact], selection: &BTreeMap<String, String>) -> Board {
    let mut filter = vec![];
    let mut unresolved = vec![];
    for (k, v) in &view.filter {
        match resolve(v, selection) {
            Some(value) => filter.push(FactInput { key: k.clone(), value, kind: None }),
            None => unresolved.push(k.clone()),
        }
    }
    let key = view.column_key.as_deref().unwrap_or("phase");
    let members: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| view.show_ended || s.state == "running")
        .filter(|s| filter.iter().all(|f| has_fact(facts, &s.id, &f.key, &f.value)))
        .collect();
    let mut columns: Vec<BoardColumn> = view
        .columns
        .iter()
        .map(|c| BoardColumn {
            value: c.clone(),
            sessions: members.iter().filter(|s| has_fact(facts, &s.id, key, c)).map(|s| s.id.clone()).collect(),
        })
        .collect();
    if view.show_unassigned {
        let rest: Vec<String> = members
            .iter()
            .filter(|s| !view.columns.iter().any(|c| has_fact(facts, &s.id, key, c)))
            .map(|s| s.id.clone())
            .collect();
        columns.insert(0, BoardColumn { value: String::new(), sessions: rest });
    }
    Board { filter, unresolved, columns }
}

pub fn chain(view: &ViewConfig, sessions: &[SessionInfo], facts: &[Fact], selection: &BTreeMap<String, String>) -> Chain {
    let scope = view.scope.as_deref().and_then(|scope| {
        let key = scope.strip_prefix("$selected.").unwrap_or(scope);
        resolve(scope, selection).map(|value| FactInput { key: key.to_string(), value, kind: None })
    });
    let Some(scope_fact) = &scope else { return Chain { scope: None, steps: vec![] } };
    let link = view.link_key.as_deref().unwrap_or("continues");
    let mut members: Vec<&SessionInfo> =
        sessions.iter().filter(|s| has_fact(facts, &s.id, &scope_fact.key, &scope_fact.value)).collect();
    members.sort_by_key(|s| s.created_at);
    let in_set = |id: &str| members.iter().any(|m| m.id == id);
    let parent = |s: &SessionInfo| fact_value(facts, &s.id, link).filter(|p| in_set(p)).map(String::from);
    // Depth-first from the roots, children in creation order.
    let mut steps = vec![];
    let mut stack: Vec<&SessionInfo> = members.iter().rev().filter(|s| parent(s).is_none()).copied().collect();
    let mut seen = HashSet::new();
    while let Some(s) = stack.pop() {
        if !seen.insert(s.id.clone()) {
            continue;
        }
        steps.push(ChainStep {
            session: s.id.clone(),
            label: view.label_key.as_deref().and_then(|k| fact_value(facts, &s.id, k)).map(String::from),
        });
        for c in members.iter().rev().filter(|c| parent(c).as_deref() == Some(s.id.as_str())) {
            stack.push(c);
        }
    }
    Chain { scope, steps }
}

pub fn render_all(sessions: &[SessionInfo], facts: &[Fact], selection: &BTreeMap<String, String>) -> Result<Vec<RenderedView>, String> {
    Ok(load()?
        .into_iter()
        .map(|v| {
            let mut r = RenderedView { view: v.clone(), root: TreeNode::default(), board: None, chain: None };
            match v.kind.as_str() {
                "tree" => r.root = tree(&v, sessions, facts),
                "board" => r.board = Some(board(&v, sessions, facts, selection)),
                "chain" => r.chain = Some(chain(&v, sessions, facts, selection)),
                _ => {}
            }
            r
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

    fn view(toml_text: &str) -> ViewConfig {
        parse(toml_text).unwrap().remove(0)
    }

    #[test]
    fn board_columns_follow_the_selection() {
        let v = view("[[view]]\nid='b'\ntype='board'\ndock='center'\ncolumn_key='phase'\ncolumns=['concept','designed']\nfilter={project='$selected.project'}\n");
        let sessions = vec![s("a", "/x", 1), s("b", "/y", 2), s("c", "/z", 3)];
        let facts = vec![f("a", "project", "tk"), f("a", "phase", "designed"), f("b", "project", "tk"), f("c", "project", "other")];
        let sel: BTreeMap<String, String> = [("project".to_string(), "tk".to_string())].into();
        let b = board(&v, &sessions, &facts, &sel);
        assert_eq!(b.columns.len(), 3); // unassigned + 2
        assert_eq!(b.columns[0].sessions, vec!["b"]);
        assert_eq!(b.columns[2].sessions, vec!["a"]);
        let none = board(&v, &sessions, &facts, &BTreeMap::new());
        assert_eq!(none.unresolved, vec!["project"]);
    }

    #[test]
    fn chain_follows_continues_links() {
        let v = view("[[view]]\nid='c'\ntype='chain'\ndock='top'\nlink_key='continues'\nlabel_key='phase'\nscope='$selected.issue'\n");
        let sessions = vec![s("a", "/x", 1), s("b", "/y", 2), s("c", "/z", 3)];
        let facts = vec![
            f("a", "issue", "T-1"), f("a", "phase", "concept"),
            f("b", "issue", "T-1"), f("b", "phase", "design"), f("b", "continues", "a"),
            f("c", "issue", "T-1"), f("c", "phase", "implementation"), f("c", "continues", "b"),
        ];
        let sel: BTreeMap<String, String> = [("issue".to_string(), "T-1".to_string())].into();
        let ch = chain(&v, &sessions, &facts, &sel);
        let order: Vec<_> = ch.steps.iter().map(|s| (s.session.as_str(), s.label.as_deref().unwrap_or(""))).collect();
        assert_eq!(order, vec![("a", "concept"), ("b", "design"), ("c", "implementation")]);
        assert!(parse("[[view]]\nid='x'\ntype='board'\n").is_err());
        assert!(parse("[[view]]\nid='x'\ntype='tree'\nlevels=['a']\ndock='side'\n").is_err());
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

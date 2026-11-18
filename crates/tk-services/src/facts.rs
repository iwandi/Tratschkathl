//! Fact store (spec §4.2) on files: every change is an event appended to the session's
//! `facts.jsonl`; the current facts are kept in memory (see `store.rs`).
//! Single-valued keys replace their old value; multi-valued keys accumulate.

use serde::{Deserialize, Serialize};

use tk_bus::model::{now_ms, Fact, FactInput};

use crate::store::Store;

/// One line of `facts.jsonl`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Event {
    Announce {
        key: String,
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        at: i64,
    },
    Clear {
        key: String,
        /// Only this value; `None` clears every value of the key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        at: i64,
    },
}

/// Keys that may hold several values at once. Everything else is single-valued.
pub fn is_multi(key: &str) -> bool {
    matches!(key, "issue" | "changelist" | "review" | "build" | "topic" | "doc")
}

pub fn validate_key(key: &str) -> Result<(), String> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'));
    if ok { Ok(()) } else { Err(format!("invalid fact key '{key}': use lowercase letters, digits, '_', '-', '.'")) }
}

/// Apply an event to the in-memory facts. Returns how many facts changed.
pub fn replay(store: &mut Store, session: &str, ev: &Event) -> usize {
    match ev {
        Event::Announce { key, value, kind, source, note, at } => {
            if store.facts.iter().any(|f| f.session == session && &f.key == key && &f.value == value && &f.kind == kind) {
                return 0;
            }
            if !is_multi(key) {
                store.facts.retain(|f| !(f.session == session && &f.key == key));
            }
            let id = store.next_id();
            store.facts.push(Fact {
                id,
                session: session.to_string(),
                key: key.clone(),
                value: value.clone(),
                kind: kind.clone(),
                source: source.clone(),
                note: note.clone(),
                announced_at: *at,
            });
            1
        }
        Event::Clear { key, value, .. } => {
            let before = store.facts.len();
            store.facts.retain(|f| !(f.session == session && &f.key == key && value.as_ref().map_or(true, |v| &f.value == v)));
            before - store.facts.len()
        }
    }
}

fn record(store: &mut Store, session: &str, ev: Event) -> Result<usize, String> {
    let n = replay(store, session, &ev);
    if n > 0 {
        store.append(session, &serde_json::to_string(&ev).map_err(|e| e.to_string())?)?;
    }
    Ok(n)
}

/// Current facts, optionally for one session and/or key, ordered by session, key, age.
pub fn current(store: &Store, session: Option<&str>, key: Option<&str>) -> Result<Vec<Fact>, String> {
    let mut v: Vec<Fact> = store
        .facts
        .iter()
        .filter(|f| session.map_or(true, |s| f.session == s) && key.map_or(true, |k| f.key == k))
        .cloned()
        .collect();
    v.sort_by(|a, b| (&a.session, &a.key, a.id).cmp(&(&b.session, &b.key, b.id)));
    Ok(v)
}

/// Announce a fact. Returns true if anything changed.
pub fn announce(store: &mut Store, session: &str, f: &FactInput, source: &str, note: Option<&str>) -> Result<bool, String> {
    validate_key(&f.key)?;
    if f.value.trim().is_empty() {
        return Err("fact value must not be empty".into());
    }
    let ev = Event::Announce {
        key: f.key.clone(),
        value: f.value.clone(),
        kind: f.kind.clone(),
        source: source.to_string(),
        note: note.map(String::from),
        at: now_ms(),
    };
    Ok(record(store, session, ev)? > 0)
}

/// Clear a key (all values) or one value of it. Returns the number of facts cleared.
pub fn clear(store: &mut Store, session: &str, key: &str, value: Option<&str>) -> Result<usize, String> {
    record(store, session, Event::Clear { key: key.to_string(), value: value.map(String::from), at: now_ms() })
}

/// Apply what a plugin derived: announce new values, clear values it no longer stands
/// behind, and never override a fact that an agent or the user set. Returns true on change.
pub fn apply_derived(store: &mut Store, session: &str, plugin: &str, derived: &[FactInput]) -> Result<bool, String> {
    let source = format!("plugin:{plugin}");
    let mut changed = false;
    let cur = current(store, Some(session), None)?;
    for f in derived {
        if cur.iter().any(|x| x.key == f.key && !x.source.starts_with("plugin:")) {
            continue;
        }
        changed |= announce(store, session, f, &source, None)?;
    }
    for x in cur.iter().filter(|x| x.source == source) {
        if !derived.iter().any(|f| f.key == x.key && f.value == x.value && f.kind == x.kind) {
            // Re-announcing a single-valued key above may already have replaced it.
            changed |= clear(store, session, &x.key, Some(&x.value))? > 0;
        }
    }
    Ok(changed)
}

/// Sessions that currently have all the given facts (`value` empty = any value of the key).
pub fn query(store: &Store, filter: &[FactInput]) -> Result<Vec<String>, String> {
    let mut sessions: Vec<String> = store.facts.iter().map(|f| f.session.clone()).collect();
    sessions.sort();
    sessions.dedup();
    Ok(sessions
        .into_iter()
        .filter(|s| {
            filter.iter().all(|want| {
                store
                    .facts
                    .iter()
                    .any(|f| &f.session == s && f.key == want.key && (want.value.is_empty() || f.value == want.value))
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(k: &str, v: &str) -> FactInput {
        FactInput { key: k.into(), value: v.into(), kind: None }
    }

    #[test]
    fn single_replaces_multi_accumulates() {
        let s = &mut Store::memory();
        assert!(announce(s, "s1", &t("phase", "concept"), "agent", None).unwrap());
        assert!(announce(s, "s1", &t("phase", "designed"), "agent", None).unwrap());
        assert!(!announce(s, "s1", &t("phase", "designed"), "agent", None).unwrap());
        announce(s, "s1", &t("issue", "A-1"), "agent", None).unwrap();
        announce(s, "s1", &t("issue", "A-2"), "agent", None).unwrap();
        let cur = current(s, Some("s1"), None).unwrap();
        assert_eq!(cur.iter().filter(|f| f.key == "phase").count(), 1);
        assert_eq!(cur.iter().filter(|f| f.key == "issue").count(), 2);
        assert_eq!(clear(s, "s1", "issue", Some("A-1")).unwrap(), 1);
        assert_eq!(current(s, Some("s1"), Some("issue")).unwrap().len(), 1);
    }

    #[test]
    fn derived_never_overrides_agent_and_clears_stale() {
        let s = &mut Store::memory();
        announce(s, "s1", &t("project", "mine"), "agent", None).unwrap();
        apply_derived(s, "s1", "git", &[t("project", "repo"), t("branch", "main")]).unwrap();
        let cur = current(s, Some("s1"), None).unwrap();
        assert_eq!(cur.iter().find(|f| f.key == "project").unwrap().value, "mine");
        assert_eq!(cur.iter().find(|f| f.key == "branch").unwrap().value, "main");
        apply_derived(s, "s1", "git", &[t("branch", "feat")]).unwrap();
        assert_eq!(current(s, Some("s1"), Some("branch")).unwrap()[0].value, "feat");
        apply_derived(s, "s1", "git", &[]).unwrap();
        assert!(current(s, Some("s1"), Some("branch")).unwrap().is_empty());
        assert_eq!(current(s, Some("s1"), Some("project")).unwrap().len(), 1);
    }

    #[test]
    fn query_matches_all_filters() {
        let s = &mut Store::memory();
        announce(s, "a", &t("project", "p"), "agent", None).unwrap();
        announce(s, "a", &t("branch", "x"), "agent", None).unwrap();
        announce(s, "b", &t("project", "p"), "agent", None).unwrap();
        assert_eq!(query(s, &[t("project", "p")]).unwrap(), vec!["a", "b"]);
        assert_eq!(query(s, &[t("project", "p"), t("branch", "x")]).unwrap(), vec!["a"]);
        assert_eq!(query(s, &[t("branch", "")]).unwrap(), vec!["a"]);
    }

    #[test]
    fn files_round_trip() {
        let root = std::env::temp_dir().join(format!("tk-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        {
            let mut s = Store::load(root.clone());
            s.put_session(tk_bus::model::SessionInfo { id: "s1".into(), state: "running".into(), ..Default::default() }).unwrap();
            announce(&mut s, "s1", &t("phase", "concept"), "agent", None).unwrap();
            announce(&mut s, "s1", &t("phase", "designed"), "agent", Some("moved")).unwrap();
            announce(&mut s, "s1", &t("issue", "A-1"), "user", None).unwrap();
            clear(&mut s, "s1", "issue", None).unwrap();
        }
        let s = Store::load(root.clone());
        assert_eq!(s.list(Some("running")).len(), 1);
        let cur = current(&s, Some("s1"), None).unwrap();
        assert_eq!(cur.len(), 1);
        assert_eq!((cur[0].key.as_str(), cur[0].value.as_str(), cur[0].note.as_deref()), ("phase", "designed", Some("moved")));
        let _ = std::fs::remove_dir_all(&root);
    }
}

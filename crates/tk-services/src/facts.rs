//! Fact store (spec §4.2): append-only rows; "current" facts are the ones not cleared.
//! Single-valued keys replace their old value; multi-valued keys accumulate.

use rusqlite::{params, Connection, OptionalExtension};

use tk_bus::model::{now_ms, Fact, FactInput};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS facts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    kind TEXT,
    source TEXT NOT NULL,
    note TEXT,
    announced_at INTEGER NOT NULL,
    cleared_at INTEGER
);
CREATE INDEX IF NOT EXISTS facts_current ON facts(session_id, key) WHERE cleared_at IS NULL;
";

/// Keys that may hold several values at once. Everything else is single-valued.
/// (A configurable key schema follows with `view_config_set` in M3.)
pub fn is_multi(key: &str) -> bool {
    matches!(key, "issue" | "changelist" | "review" | "build" | "topic" | "doc")
}

pub fn validate_key(key: &str) -> Result<(), String> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'));
    if ok { Ok(()) } else { Err(format!("invalid fact key '{key}': use lowercase letters, digits, '_', '-', '.'")) }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: r.get(0)?,
        session: r.get(1)?,
        key: r.get(2)?,
        value: r.get(3)?,
        kind: r.get(4)?,
        source: r.get(5)?,
        note: r.get(6)?,
        announced_at: r.get(7)?,
    })
}

const COLS: &str = "id, session_id, key, value, kind, source, note, announced_at";

/// Current facts, optionally for one session and/or key.
pub fn current(db: &Connection, session: Option<&str>, key: Option<&str>) -> rusqlite::Result<Vec<Fact>> {
    let mut stmt = db.prepare(&format!(
        "SELECT {COLS} FROM facts WHERE cleared_at IS NULL
           AND (?1 IS NULL OR session_id = ?1) AND (?2 IS NULL OR key = ?2)
         ORDER BY session_id, key, id"
    ))?;
    let rows = stmt.query_map(params![session, key], row)?;
    rows.collect()
}

/// Announce a fact. Returns true if anything changed.
pub fn announce(
    db: &Connection,
    session: &str,
    f: &FactInput,
    source: &str,
    note: Option<&str>,
) -> Result<bool, String> {
    validate_key(&f.key)?;
    if f.value.trim().is_empty() {
        return Err("fact value must not be empty".into());
    }
    let existing = current(db, Some(session), Some(&f.key)).map_err(|e| e.to_string())?;
    if existing.iter().any(|x| x.value == f.value && x.kind == f.kind) {
        return Ok(false);
    }
    let now = now_ms();
    if !is_multi(&f.key) {
        db.execute(
            "UPDATE facts SET cleared_at = ?1 WHERE session_id = ?2 AND key = ?3 AND cleared_at IS NULL",
            params![now, session, f.key],
        )
        .map_err(|e| e.to_string())?;
    }
    db.execute(
        "INSERT INTO facts (session_id, key, value, kind, source, note, announced_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![session, f.key, f.value, f.kind, source, note, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Clear a key (all values) or one value of it. Returns the number of facts cleared.
pub fn clear(db: &Connection, session: &str, key: &str, value: Option<&str>) -> Result<usize, String> {
    db.execute(
        "UPDATE facts SET cleared_at = ?1
          WHERE session_id = ?2 AND key = ?3 AND cleared_at IS NULL AND (?4 IS NULL OR value = ?4)",
        params![now_ms(), session, key, value],
    )
    .map_err(|e| e.to_string())
}

/// Apply what a plugin derived: announce new values, clear values it no longer stands
/// behind, and never override a fact that an agent or the user set. Returns true on change.
pub fn apply_derived(db: &Connection, session: &str, plugin: &str, derived: &[FactInput]) -> Result<bool, String> {
    let source = format!("plugin:{plugin}");
    let mut changed = false;
    let cur = current(db, Some(session), None).map_err(|e| e.to_string())?;
    for f in derived {
        let owned_by_others = cur.iter().any(|x| x.key == f.key && !x.source.starts_with("plugin:"));
        if owned_by_others {
            continue;
        }
        changed |= announce(db, session, f, &source, None)?;
    }
    for x in cur.iter().filter(|x| x.source == source) {
        if !derived.iter().any(|f| f.key == x.key && f.value == x.value && f.kind == x.kind) {
            // A single-valued key that was re-announced above is already cleared.
            let still = db
                .query_row("SELECT 1 FROM facts WHERE id = ?1 AND cleared_at IS NULL", params![x.id], |_| Ok(()))
                .optional()
                .map_err(|e| e.to_string())?;
            if still.is_some() {
                db.execute("UPDATE facts SET cleared_at = ?1 WHERE id = ?2", params![now_ms(), x.id])
                    .map_err(|e| e.to_string())?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Sessions that currently have all the given facts (`value` empty = any value of the key).
pub fn query(db: &Connection, filter: &[FactInput]) -> Result<Vec<String>, String> {
    let all = current(db, None, None).map_err(|e| e.to_string())?;
    let mut sessions: Vec<String> = all.iter().map(|f| f.session.clone()).collect();
    sessions.sort();
    sessions.dedup();
    Ok(sessions
        .into_iter()
        .filter(|s| {
            filter.iter().all(|want| {
                all.iter().any(|f| {
                    &f.session == s && f.key == want.key && (want.value.is_empty() || f.value == want.value)
                })
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }
    fn t(k: &str, v: &str) -> FactInput {
        FactInput { key: k.into(), value: v.into(), kind: None }
    }

    #[test]
    fn single_replaces_multi_accumulates() {
        let db = db();
        assert!(announce(&db, "s1", &t("phase", "concept"), "agent", None).unwrap());
        assert!(announce(&db, "s1", &t("phase", "designed"), "agent", None).unwrap());
        assert!(!announce(&db, "s1", &t("phase", "designed"), "agent", None).unwrap());
        announce(&db, "s1", &t("issue", "A-1"), "agent", None).unwrap();
        announce(&db, "s1", &t("issue", "A-2"), "agent", None).unwrap();
        let cur = current(&db, Some("s1"), None).unwrap();
        assert_eq!(cur.iter().filter(|f| f.key == "phase").count(), 1);
        assert_eq!(cur.iter().filter(|f| f.key == "issue").count(), 2);
        assert_eq!(clear(&db, "s1", "issue", Some("A-1")).unwrap(), 1);
        assert_eq!(current(&db, Some("s1"), Some("issue")).unwrap().len(), 1);
    }

    #[test]
    fn derived_never_overrides_agent_and_clears_stale() {
        let db = db();
        announce(&db, "s1", &t("project", "mine"), "agent", None).unwrap();
        apply_derived(&db, "s1", "git", &[t("project", "repo"), t("branch", "main")]).unwrap();
        let cur = current(&db, Some("s1"), None).unwrap();
        assert_eq!(cur.iter().find(|f| f.key == "project").unwrap().value, "mine");
        assert_eq!(cur.iter().find(|f| f.key == "branch").unwrap().value, "main");
        apply_derived(&db, "s1", "git", &[t("branch", "feat")]).unwrap();
        assert_eq!(current(&db, Some("s1"), Some("branch")).unwrap()[0].value, "feat");
        apply_derived(&db, "s1", "git", &[]).unwrap();
        assert!(current(&db, Some("s1"), Some("branch")).unwrap().is_empty());
        assert_eq!(current(&db, Some("s1"), Some("project")).unwrap().len(), 1);
    }

    #[test]
    fn query_matches_all_filters() {
        let db = db();
        announce(&db, "a", &t("project", "p"), "agent", None).unwrap();
        announce(&db, "a", &t("branch", "x"), "agent", None).unwrap();
        announce(&db, "b", &t("project", "p"), "agent", None).unwrap();
        assert_eq!(query(&db, &[t("project", "p")]).unwrap(), vec!["a", "b"]);
        assert_eq!(query(&db, &[t("project", "p"), t("branch", "x")]).unwrap(), vec!["a"]);
        assert_eq!(query(&db, &[t("branch", "")]).unwrap(), vec!["a"]);
    }
}

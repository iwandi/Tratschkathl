//! Decision log (spec §7.1). Agents log decisions with `decision_log`; Core's automatic
//! decisions (reloads, rollbacks) arrive through `<home>/decisions.jsonl`, which Services
//! imports. Each decision keeps a snapshot of its session's facts, so it can be filtered by
//! project or issue and shown as a `decision-list` document.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use tk_bus::model::now_ms;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    session TEXT,
    source TEXT NOT NULL,
    title TEXT NOT NULL,
    context TEXT,
    decision TEXT,
    alternatives TEXT,
    rationale TEXT,
    facts TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Decision {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub source: String,
    pub title: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub alternatives: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub facts: BTreeMap<String, String>,
}

pub fn log(db: &Connection, d: &Decision) -> Result<i64, String> {
    if d.title.trim().is_empty() {
        return Err("a decision needs a title".into());
    }
    db.execute(
        "INSERT INTO decisions (at, session, source, title, context, decision, alternatives, rationale, facts)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            if d.at > 0 { d.at } else { now_ms() },
            d.session,
            d.source,
            d.title,
            d.context,
            d.decision,
            d.alternatives,
            d.rationale,
            serde_json::to_string(&d.facts).unwrap_or_else(|_| "{}".into())
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(db.last_insert_rowid())
}

/// Decisions whose fact snapshot matches every filter pair, newest first.
/// `source` filters by source (e.g. `core`).
pub fn list(db: &Connection, filter: &BTreeMap<String, String>, source: Option<&str>, limit: usize) -> Result<Vec<Decision>, String> {
    let mut stmt = db
        .prepare(
            "SELECT id, at, session, source, title, context, decision, alternatives, rationale, facts
             FROM decisions WHERE (?1 IS NULL OR source = ?1) ORDER BY at DESC, id DESC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![source], |r| {
            let facts: String = r.get(9)?;
            Ok(Decision {
                id: r.get(0)?,
                at: r.get(1)?,
                session: r.get(2)?,
                source: r.get(3)?,
                title: r.get(4)?,
                context: r.get(5)?,
                decision: r.get(6)?,
                alternatives: r.get(7)?,
                rationale: r.get(8)?,
                facts: serde_json::from_str(&facts).unwrap_or_default(),
            })
        })
        .map_err(|e| e.to_string())?;
    Ok(rows
        .filter_map(Result::ok)
        .filter(|d| filter.iter().all(|(k, v)| d.facts.get(k) == Some(v)))
        .take(limit)
        .collect())
}

/// Import new lines of Core's `decisions.jsonl` (it writes there because it must work while
/// Services is down). Returns how many were imported.
pub fn import_core(db: &Connection) -> usize {
    let path = tk_bus::paths::home().join("decisions.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else { return 0 };
    let done: usize = db
        .query_row("SELECT value FROM kv WHERE key = 'core_decisions_lines'", [], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= done {
        return 0;
    }
    let mut n = 0;
    for l in &lines[done..] {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else { continue };
        let d = Decision {
            at: v["at"].as_i64().unwrap_or(0),
            source: v["source"].as_str().unwrap_or("core").to_string(),
            title: v["title"].as_str().unwrap_or("").to_string(),
            rationale: v["rationale"].as_str().map(String::from),
            ..Default::default()
        };
        if log(db, &d).is_ok() {
            n += 1;
        }
    }
    let _ = db.execute(
        "INSERT OR REPLACE INTO kv (key, value) VALUES ('core_decisions_lines', ?1)",
        params![lines.len().to_string()],
    );
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_and_filter_by_facts() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        let mut facts = BTreeMap::new();
        facts.insert("project".to_string(), "tk".to_string());
        log(&db, &Decision { title: "Use SQLite".into(), source: "agent".into(), facts: facts.clone(), ..Default::default() }).unwrap();
        log(&db, &Decision { title: "Other".into(), source: "agent".into(), ..Default::default() }).unwrap();
        assert_eq!(list(&db, &facts, None, 10).unwrap().len(), 1);
        assert_eq!(list(&db, &BTreeMap::new(), None, 10).unwrap().len(), 2);
        assert!(log(&db, &Decision::default()).is_err());
    }
}

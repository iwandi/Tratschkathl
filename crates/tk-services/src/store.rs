//! Host state as files (no database): everything about a session lives in its folder
//! `<home>/sessions/<id>/`:
//! - `session.json`: the session record
//! - `facts.jsonl`: append-only fact events (announce / clear), replayed at start
//!
//! Services keeps the current state in memory; the files are the truth.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use tk_bus::model::{Fact, SessionInfo};

pub struct Store {
    pub sessions: BTreeMap<String, SessionInfo>,
    /// Current (not cleared) facts of all sessions.
    pub facts: Vec<Fact>,
    next_id: i64,
    /// `None` keeps everything in memory (tests).
    root: Option<PathBuf>,
}

impl Store {
    pub fn memory() -> Store {
        Store { sessions: BTreeMap::new(), facts: vec![], next_id: 1, root: None }
    }

    /// Load every session folder under `root` and replay its fact log.
    pub fn load(root: PathBuf) -> Store {
        let mut store = Store { root: Some(root.clone()), ..Store::memory() };
        let Ok(rd) = std::fs::read_dir(&root) else { return store };
        for dir in rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let Some(info) = std::fs::read(dir.join("session.json")).ok().and_then(|b| serde_json::from_slice::<SessionInfo>(&b).ok()) else {
                continue;
            };
            let id = info.id.clone();
            store.sessions.insert(id.clone(), info);
            if let Ok(text) = std::fs::read_to_string(dir.join("facts.jsonl")) {
                for line in text.lines() {
                    if let Ok(ev) = serde_json::from_str(line) {
                        crate::facts::replay(&mut store, &id, &ev);
                    }
                }
            }
        }
        store
    }

    pub fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Append one fact event to the session's log.
    pub fn append(&self, session: &str, line: &str) -> Result<(), String> {
        let Some(root) = &self.root else { return Ok(()) };
        let dir = root.join(session);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("facts.jsonl"))
            .map_err(|e| e.to_string())?;
        writeln!(f, "{line}").map_err(|e| e.to_string())
    }

    /// Insert or replace a session record.
    pub fn put_session(&mut self, s: SessionInfo) -> Result<(), String> {
        if let Some(root) = &self.root {
            let path = root.join(&s.id).join("session.json");
            let bytes = serde_json::to_vec_pretty(&s).map_err(|e| e.to_string())?;
            tk_bus::util::write_atomic(&path, &bytes).map_err(|e| e.to_string())?;
        }
        self.sessions.insert(s.id.clone(), s);
        Ok(())
    }

    /// Sessions, newest first, optionally only those in `state`.
    pub fn list(&self, state: Option<&str>) -> Vec<SessionInfo> {
        let mut v: Vec<SessionInfo> = self.sessions.values().filter(|s| state.map_or(true, |st| s.state == st)).cloned().collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        v
    }
}

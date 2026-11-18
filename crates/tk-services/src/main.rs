//! L2 Services (thin for M0): session registry in SQLite and the tool registry. Facts, views,
//! decisions, automation and resources arrive here in later milestones.

use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use tk_bus::model::SessionInfo;
use tk_bus::{client::handler, Bus, Incoming};

struct Services {
    db: Mutex<Connection>,
    bus: std::sync::OnceLock<Bus>,
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        // Prove the DB opens and migrates.
        match open_db() {
            Ok(_) => println!("{}", tk_bus::self_test_json("services")),
            Err(e) => {
                eprintln!("db: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    tk_bus::logging::init("services");
    let db = match open_db() {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("cannot open db: {e}");
            std::process::exit(1);
        }
    };
    let svc = Arc::new(Services { db: Mutex::new(db), bus: Default::default() });
    let h = {
        let svc = svc.clone();
        handler(move |inc| {
            let svc = svc.clone();
            async move { svc.handle(inc) }
        })
    };
    let (bus, _events) = Bus::connect("services", env!("CARGO_PKG_VERSION"), vec![], Some(h));
    let _ = svc.bus.set(bus);
    tracing::info!("services up");
    std::future::pending::<()>().await;
}

fn open_db() -> rusqlite::Result<Connection> {
    let _ = std::fs::create_dir_all(tk_bus::paths::home());
    let db = Connection::open(tk_bus::paths::home().join("tratschkathl.db"))?;
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            agent TEXT NOT NULL,
            title TEXT NOT NULL,
            cwd TEXT NOT NULL,
            state TEXT NOT NULL,
            created_at INTEGER NOT NULL
         );",
    )?;
    Ok(db)
}

impl Services {
    fn emit(&self, topic: &str, data: Value) {
        if let Some(bus) = self.bus.get() {
            bus.emit(topic, data);
        }
    }

    fn handle(&self, inc: Incoming) -> Result<Value, String> {
        let p = &inc.params;
        match inc.method.as_str() {
            "layer.shutdown" => {
                tk_bus::client::exit_soon(0);
                Ok(json!(true))
            }
            "session.register" => {
                let s: SessionInfo = serde_json::from_value(p.clone()).map_err(|e| e.to_string())?;
                self.db
                    .lock()
                    .unwrap()
                    .execute(
                        "INSERT OR REPLACE INTO sessions (id, kind, agent, title, cwd, state, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                        params![s.id, s.kind, s.agent, s.title, s.cwd, s.state, s.created_at],
                    )
                    .map_err(|e| e.to_string())?;
                self.emit("sessions.changed", json!({ "id": s.id }));
                Ok(json!(true))
            }
            "session.update" => {
                let id = p["id"].as_str().ok_or("id missing")?;
                let db = self.db.lock().unwrap();
                if let Some(state) = p["state"].as_str() {
                    db.execute("UPDATE sessions SET state=?1 WHERE id=?2", params![state, id]).map_err(|e| e.to_string())?;
                }
                if let Some(title) = p["title"].as_str() {
                    db.execute("UPDATE sessions SET title=?1 WHERE id=?2", params![title, id]).map_err(|e| e.to_string())?;
                }
                drop(db);
                self.emit("sessions.changed", json!({ "id": id }));
                Ok(json!(true))
            }
            "session.list" => Ok(serde_json::to_value(self.sessions(p["state"].as_str())?).unwrap()),
            "tools.list" => Ok(tool_defs()),
            "tool.call" => {
                let name = p["name"].as_str().unwrap_or("");
                match name {
                    "session_list" => Ok(serde_json::to_value(self.sessions(p["args"]["state"].as_str())?).unwrap()),
                    "session_focus" => {
                        let id = p["args"]["session"].as_str().or(p["session"].as_str()).ok_or("session missing")?;
                        if !self.sessions(None)?.iter().any(|s| s.id == id) {
                            return Err(format!("unknown session {id}"));
                        }
                        self.emit("workbench.focus", json!({ "session": id }));
                        Ok(json!({ "focused": id }))
                    }
                    other => Err(format!("unknown tool {other}")),
                }
            }
            other => Err(format!("services: unknown method {other}")),
        }
    }

    fn sessions(&self, state: Option<&str>) -> Result<Vec<SessionInfo>, String> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT id, kind, agent, title, cwd, state, created_at FROM sessions ORDER BY created_at DESC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SessionInfo {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    agent: r.get(2)?,
                    title: r.get(3)?,
                    cwd: r.get(4)?,
                    state: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows
            .filter_map(Result::ok)
            .filter(|s| state.map_or(true, |st| s.state == st))
            .collect())
    }
}

/// Tool registry exposed to agents (MCP for terminal agents, ACP tools later).
fn tool_defs() -> Value {
    json!([
        {
            "name": "host_status",
            "description": "Show the running Tratschkathl layers (core, services, agents, ui), their versions (current / previous / last known good) and the source checkout used for reloads.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "host_reload",
            "description": "Rebuild Tratschkathl from its source checkout and reload the layers whose binaries changed (bottom-up: helpers, core, services, agents, ui). Each new binary must pass a self-test first; a layer that does not come up is rolled back. Agent sessions, including yours, survive the reload. Returns per-layer versions, self-test output, or build errors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "layers": { "type": "array", "items": { "type": "string", "enum": ["helpers", "core", "services", "agents", "ui"] },
                                "description": "Limit the reload to these units. Default: every unit whose binary changed." },
                    "reason": { "type": "string", "description": "Why you are reloading; goes to the decision log." }
                }
            }
        },
        {
            "name": "host_rollback",
            "description": "Roll one layer back to its last known good (or previous) version.",
            "inputSchema": {
                "type": "object",
                "properties": { "layer": { "type": "string", "enum": ["core", "services", "agents", "ui", "helpers"] } },
                "required": ["layer"]
            }
        },
        {
            "name": "session_focus",
            "description": "Bring a session to the front of the user's workbench (opens or activates its tab). Without a session id it focuses your own session.",
            "inputSchema": { "type": "object", "properties": { "session": { "type": "string" } } }
        },
        {
            "name": "session_list",
            "description": "List agent sessions known to Tratschkathl.",
            "inputSchema": { "type": "object", "properties": { "state": { "type": "string", "enum": ["running", "ended"] } } }
        }
    ])
}

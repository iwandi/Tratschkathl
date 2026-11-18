//! L2 Services: session registry, fact store, view engine, tool registry, and the plugin host
//! for non-agent plugins (Git: facts and changesets; files: Markdown documents).

mod commands;
mod decisions;
mod facts;
mod files;
mod tools;
mod versioned;
mod views;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use tk_bus::model::{parse_doc_uri, DocRef, Document, FactInput, SessionInfo};
use tk_bus::{client::handler, Bus, Incoming};
use tk_plugin_api::{Plugin, SessionContext};

pub struct Services {
    db: Mutex<Connection>,
    bus: OnceLock<Bus>,
    plugins: Vec<Box<dyn Plugin>>,
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        // Prove the DB opens and migrates, and the view config parses.
        if let Err(e) = open_db() {
            eprintln!("db: {e}");
            std::process::exit(1);
        }
        if let Err(e) = views::load() {
            eprintln!("{e}");
            std::process::exit(1);
        }
        println!("{}", tk_bus::self_test_json("services"));
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
    let svc = Arc::new(Services { db: Mutex::new(db), bus: OnceLock::new(), plugins: plugins() });
    let h = {
        let svc = svc.clone();
        handler(move |inc| {
            let svc = svc.clone();
            async move { svc.handle(inc).await }
        })
    };
    let (bus, _events) = Bus::connect("services", env!("CARGO_PKG_VERSION"), vec![], Some(h));
    let _ = svc.bus.set(bus);
    tracing::info!("services up with plugins {:?}", svc.plugins.iter().map(|p| p.id()).collect::<Vec<_>>());

    // Fact derivation for running sessions, and views.toml change detection.
    tokio::spawn(svc.clone().background());
    std::future::pending::<()>().await;
}

/// Plugins compiled into this layer (spec §5.2, v1 model).
fn plugins() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(tk_plugin_git::GitPlugin), Box::new(files::FilesPlugin)]
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
    db.execute_batch(facts::SCHEMA)?;
    Ok(db)
}

impl Services {
    pub fn emit(&self, topic: &str, data: Value) {
        if let Some(bus) = self.bus.get() {
            bus.emit(topic, data);
        }
    }

    async fn handle(self: &Arc<Self>, inc: Incoming) -> Result<Value, String> {
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
                let me = self.clone();
                tokio::spawn(async move { me.derive_for(&s).await });
                Ok(json!(true))
            }
            "session.update" => {
                let id = p["id"].as_str().ok_or("id missing")?;
                {
                    let db = self.db.lock().unwrap();
                    if let Some(state) = p["state"].as_str() {
                        db.execute("UPDATE sessions SET state=?1 WHERE id=?2", params![state, id]).map_err(|e| e.to_string())?;
                    }
                    if let Some(title) = p["title"].as_str() {
                        db.execute("UPDATE sessions SET title=?1 WHERE id=?2", params![title, id]).map_err(|e| e.to_string())?;
                    }
                }
                self.emit("sessions.changed", json!({ "id": id }));
                Ok(json!(true))
            }
            "session.list" => Ok(serde_json::to_value(self.sessions(p["state"].as_str())?).unwrap()),
            "fact.seed" => {
                // Facts a new session starts with (from "+" in a view), recorded as user facts.
                let session = p["session"].as_str().ok_or("session missing")?;
                let list: Vec<FactInput> = serde_json::from_value(p["facts"].clone()).unwrap_or_default();
                let source = p["source"].as_str().unwrap_or("user");
                {
                    let db = self.db.lock().unwrap();
                    for f in &list {
                        facts::announce(&db, session, f, source, None)?;
                    }
                }
                self.facts_changed(session);
                Ok(json!(list.len()))
            }
            "fact.list" => {
                let db = self.db.lock().unwrap();
                let list = facts::current(&db, p["session"].as_str(), p["key"].as_str()).map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(list).unwrap())
            }
            "view.render" => {
                let sessions = self.sessions(None)?;
                let all = facts::current(&self.db.lock().unwrap(), None, None).map_err(|e| e.to_string())?;
                let selection: std::collections::BTreeMap<String, String> =
                    serde_json::from_value(p["selection"].clone()).unwrap_or_default();
                Ok(serde_json::to_value(views::render_all(&sessions, &all, &selection)?).unwrap())
            }
            "document.list" => {
                let session = p["session"].as_str().ok_or("session missing")?;
                Ok(serde_json::to_value(self.documents_for(session)?).unwrap())
            }
            "document.get" => {
                let uri = p["uri"].as_str().ok_or("uri missing")?.to_string();
                let me = self.clone();
                let doc = tokio::task::spawn_blocking(move || me.document(&uri)).await.map_err(|e| e.to_string())??;
                Ok(serde_json::to_value(doc).unwrap())
            }
            "command.list" => Ok(serde_json::to_value(commands::parse(&commands::FILE.text(), &tools::names())?).unwrap()),
            "command.run" => self.run_command(p).await,
            "tools.list" => Ok(tools::defs()),
            "tool.call" => tools::call(self, p),
            other => Err(format!("services: unknown method {other}")),
        }
    }

    /// Documents every provider offers for a session's current facts (spec §4.4).
    pub fn documents_for(&self, session: &str) -> Result<Vec<DocRef>, String> {
        let current = facts::current(&self.db.lock().unwrap(), Some(session), None).map_err(|e| e.to_string())?;
        let mut out: Vec<DocRef> = vec![];
        // Built in: the project's decision log (.tk/decisions), and the host's own decisions.
        if let Some(project) = self.project_root(session) {
            let name = current.iter().find(|f| f.key == "project").map(|f| f.value.clone()).unwrap_or_else(|| {
                std::path::Path::new(&project).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            });
            out.push(DocRef {
                uri: tk_bus::model::doc_uri("decision-list", "decisions", &project),
                kind: "decision-list".into(),
                title: format!("Decisions · {name}"),
                provider: "decisions".into(),
            });
        }
        out.push(DocRef {
            uri: tk_bus::model::doc_uri("decision-list", "decisions", "host"),
            kind: "decision-list".into(),
            title: "Host decisions".into(),
            provider: "decisions".into(),
        });
        for p in &self.plugins {
            if let Some(d) = p.document_provider() {
                for r in d.list(&current) {
                    if !out.iter().any(|x| x.uri == r.uri) {
                        out.push(r);
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn document(&self, uri: &str) -> Result<Document, String> {
        let (kind, provider, reference) = parse_doc_uri(uri).ok_or_else(|| format!("not a document uri: {uri}"))?;
        if provider == "decisions" {
            let (title, entries) = if reference == "host" {
                ("Host decisions".to_string(), decisions::host(200))
            } else {
                let project = std::path::PathBuf::from(reference);
                let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                (format!("Decisions · {name}"), decisions::list(&project, &Default::default(), 500))
            };
            return Ok(Document {
                uri: uri.to_string(),
                kind: kind.to_string(),
                title,
                body: json!({ "project": reference, "dir": if reference == "host" { String::new() } else { decisions::dir(std::path::Path::new(reference)).to_string_lossy().into_owned() }, "entries": entries }),
            });
        }
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.id() == provider)
            .ok_or_else(|| format!("no plugin '{provider}'"))?;
        plugin
            .document_provider()
            .ok_or_else(|| format!("plugin '{provider}' provides no documents"))?
            .get(kind, reference)
    }

    /// Run an agent-configured UI command for the selected session (spec §6.1).
    async fn run_command(self: &Arc<Self>, p: &Value) -> Result<Value, String> {
        let id = p["id"].as_str().ok_or("id missing")?;
        let session = p["session"].as_str().unwrap_or("").to_string();
        let cmds = commands::parse(&commands::FILE.text(), &tools::names())?;
        let cmd = cmds.into_iter().find(|c| c.id == id).ok_or_else(|| format!("no command '{id}'"))?;
        let info = self.sessions(None)?.into_iter().find(|s| s.id == session);
        let cwd = info.as_ref().map(|s| s.cwd.clone()).unwrap_or_default();
        let mut facts_map = std::collections::BTreeMap::new();
        for f in facts::current(&self.db.lock().unwrap(), Some(&session), None).map_err(|e| e.to_string())? {
            facts_map.entry(f.key).or_insert(f.value);
        }
        let action = commands::fill(&cmd.action, &session, &cwd, &facts_map);
        let bus = self.bus.get().ok_or("bus not ready")?.clone();
        if let Some(tool) = action["tool"].as_str() {
            let req = json!({ "session": "", "name": tool, "args": action["args"], "idem": format!("cmd-{id}-{}", tk_bus::model::now_ms()) });
            if tool.starts_with("host_") {
                bus.request_timeout("core", "tool.call", req, Duration::from_secs(1800)).await
            } else {
                tools::call(self, &req)
            }
        } else if let Some(text) = action["prompt"].as_str() {
            if session.is_empty() {
                return Err("select a session first".into());
            }
            bus.request_timeout("agents", "session.send_text", json!({ "session": session, "text": text }), Duration::from_secs(30)).await
        } else if let Some(start) = action.get("start_session") {
            let facts: Vec<FactInput> = start["facts"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().filter(|v| !v.is_empty()).map(|v| FactInput { key: k.clone(), value: v.to_string(), kind: None }))
                        .collect()
                })
                .unwrap_or_default();
            let req = json!({ "agent": start["agent"], "cwd": if cwd.is_empty() { Value::Null } else { json!(cwd) }, "facts": facts });
            bus.request_timeout("agents", "session.create", req, Duration::from_secs(60)).await
        } else {
            Err("command has no action".into())
        }
    }

    /// The project a session works in: its git worktree, or its folder outside git.
    pub fn project_root(&self, session: &str) -> Option<String> {
        let current = facts::current(&self.db.lock().unwrap(), Some(session), Some("worktree")).ok()?;
        if let Some(w) = current.first() {
            return Some(w.value.clone());
        }
        self.sessions(None).ok()?.into_iter().find(|s| s.id == session).map(|s| s.cwd.replace('\\', "/"))
    }

    pub fn facts_changed(&self, session: &str) {
        self.emit("facts.changed", json!({ "session": session }));
    }

    pub fn sessions(&self, state: Option<&str>) -> Result<Vec<SessionInfo>, String> {
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

    /// Run every fact deriver for one session and apply the result.
    async fn derive_for(self: &Arc<Self>, s: &SessionInfo) {
        let me = self.clone();
        let ctx = SessionContext { session: s.id.clone(), cwd: PathBuf::from(&s.cwd) };
        let changed = tokio::task::spawn_blocking(move || {
            let mut changed = false;
            for plugin in &me.plugins {
                let Some(d) = plugin.fact_deriver() else { continue };
                let derived = d.derive(&ctx);
                let db = me.db.lock().unwrap();
                match facts::apply_derived(&db, &ctx.session, plugin.id(), &derived) {
                    Ok(c) => changed |= c,
                    Err(e) => tracing::warn!("{}: deriving facts for {} failed: {e}", plugin.id(), ctx.session),
                }
            }
            changed
        })
        .await
        .unwrap_or(false);
        if changed {
            self.facts_changed(&s.id);
        }
    }

    async fn background(self: Arc<Self>) {
        let mut views_mtime: Option<SystemTime> = None;
        let mut tick: u64 = 0;
        loop {
            // views.toml edits (by hand or by an agent) show up without a restart.
            let m = std::fs::metadata(views::path()).and_then(|m| m.modified()).ok();
            if views_mtime.is_some() && m != views_mtime {
                self.emit("views.changed", json!({}));
            }
            views_mtime = m;

            // Re-derive facts every 10 s: branches change under running sessions.
            if tick % 5 == 0 {
                if let Ok(running) = self.sessions(Some("running")) {
                    for s in running {
                        self.derive_for(&s).await;
                    }
                }
            }
            tick += 1;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

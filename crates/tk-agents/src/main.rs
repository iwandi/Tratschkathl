//! L3 Agent Handler: session lifecycle (start, attach, kill), terminal emulation, and the ACP
//! client for chat sessions. Agents run under `tk-shim`, so this layer can reload at any time:
//! it re-attaches to every shim and rebuilds terminal state (ring buffer replay) or the chat
//! transcript (full ACP log replay).

mod acp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use interprocess::local_socket::tokio::{prelude::*, Stream};
use interprocess::local_socket::GenericNamespaced;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use tk_bus::frame::KeyInput;
use tk_bus::model::{now_ms, AgentConfig, SessionInfo};
use tk_bus::settings::Settings;
use tk_bus::versions::VersionState;
use tk_bus::{client::handler, paths, Bus, Incoming};
use tk_term::TermModel;

struct Sess {
    info: SessionInfo,
    model: TermModel,
    /// Chat (ACP) sessions only.
    chat: Option<acp::Chat>,
    shim_tx: Option<mpsc::UnboundedSender<String>>,
    dirty: bool,
    replaying: bool,
    fresh: bool,
    last_title: Option<String>,
}

struct Agents {
    bus: OnceLock<Bus>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Sess>>>>,
    settings: Settings,
    counter: AtomicU64,
}

fn b64d(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap_or_default()
}
fn b64e(d: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(d)
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        let _ = TermModel::new(80, 24).frame();
        println!("{}", tk_bus::self_test_json("agents"));
        return;
    }
    tk_bus::logging::init("agents");
    let agents = Arc::new(Agents {
        bus: OnceLock::new(),
        sessions: Mutex::new(HashMap::new()),
        settings: Settings::load(),
        counter: AtomicU64::new(0),
    });
    let h = {
        let a = agents.clone();
        handler(move |inc| {
            let a = a.clone();
            async move { a.handle(inc).await }
        })
    };
    let (bus, mut events) = Bus::connect("agents", env!("CARGO_PKG_VERSION"), vec!["ui.".into()], Some(h));
    let _ = agents.bus.set(bus.clone());

    // UI input events.
    {
        let a = agents.clone();
        tokio::spawn(async move {
            while let Some((topic, data)) = events.recv().await {
                a.on_ui_event(&topic, &data);
            }
        });
    }

    // Frame pump: send changed terminals to the UI at most ~30 times a second.
    {
        let a = agents.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(33)).await;
                a.pump();
            }
        });
    }

    // Re-attach to sessions that are still running (after a reload or restart).
    match bus.request_timeout("services", "session.list", json!({ "state": "running" }), Duration::from_secs(60)).await {
        Ok(list) => {
            let list: Vec<SessionInfo> = serde_json::from_value(list).unwrap_or_default();
            for info in list {
                tracing::info!("re-attaching {}", info.id);
                agents.add_session(info, 120, 36);
            }
        }
        Err(e) => tracing::error!("cannot list sessions: {e}"),
    }
    tracing::info!("agents up");
    std::future::pending::<()>().await;
}

impl Agents {
    fn bus(&self) -> &Bus {
        self.bus.get().expect("bus set")
    }

    fn get(&self, id: &str) -> Option<Arc<Mutex<Sess>>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    async fn handle(self: &Arc<Self>, inc: Incoming) -> Result<Value, String> {
        let p = &inc.params;
        match inc.method.as_str() {
            "layer.shutdown" => {
                tk_bus::client::exit_soon(0);
                Ok(json!(true))
            }
            "agents.list" => Ok(serde_json::to_value(&self.settings.agents).unwrap()),
            "session.create" => {
                let agent = p["agent"].as_str().ok_or("agent missing")?;
                let cwd = p["cwd"].as_str().map(String::from).unwrap_or_else(|| {
                    dirs_home().to_string_lossy().into_owned()
                });
                let cols = p["cols"].as_u64().unwrap_or(120) as u16;
                let rows = p["rows"].as_u64().unwrap_or(36) as u16;
                let info = self.create(agent, &cwd, cols, rows).await?;
                // Facts the session starts with ("+" on a view node): spec §4.3.
                if let Some(facts) = p.get("facts").filter(|f| f.as_array().is_some_and(|a| !a.is_empty())) {
                    let source = if inc.from == "ui" { "user" } else { "agent" };
                    self.bus()
                        .request_timeout(
                            "services",
                            "fact.seed",
                            json!({ "session": info.id, "facts": facts, "source": source }),
                            Duration::from_secs(30),
                        )
                        .await?;
                }
                Ok(serde_json::to_value(info).unwrap())
            }
            "session.kill" => {
                let id = p["session"].as_str().ok_or("session missing")?;
                self.send_shim(id, json!({ "t": "kill" }));
                Ok(json!(true))
            }
            "agents.kill_all" => {
                let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
                for id in ids {
                    self.send_shim(&id, json!({ "t": "kill" }));
                }
                Ok(json!(true))
            }
            "term.refresh" => {
                for s in self.sessions.lock().unwrap().values() {
                    let mut s = s.lock().unwrap();
                    s.dirty = true;
                    if let Some(c) = &mut s.chat {
                        c.dirty_from = Some(0);
                    }
                }
                Ok(json!(true))
            }
            "chat.send" | "chat.cancel" | "chat.permission" => {
                let id = p["session"].as_str().ok_or("session missing")?;
                let sess = self.get(id).ok_or("session is not running")?;
                let mut s = sess.lock().unwrap();
                let tx = s.shim_tx.clone().ok_or("session not attached yet")?;
                let chat = s.chat.as_mut().ok_or("not a chat session")?;
                let out = match inc.method.as_str() {
                    "chat.send" => chat.prompt(p["text"].as_str().unwrap_or(""))?,
                    "chat.cancel" => chat.cancel(),
                    _ => chat.choose(p["request"].as_str().ok_or("request missing")?, p["option"].as_str()),
                };
                for l in out {
                    let _ = tx.send(json!({ "t": "send", "line": l }).to_string() + "\n");
                }
                Ok(json!({ "state": chat.state }))
            }
            "chat.history" => {
                // Transcript of a session that is no longer running, rebuilt from its ACP log.
                let id = p["session"].as_str().ok_or("session missing")?;
                if let Some(sess) = self.get(id) {
                    if let Some(c) = &sess.lock().unwrap().chat {
                        return Ok(json!({ "session": id, "from": 0, "items": c.items, "state": c.state }));
                    }
                }
                let text = std::fs::read_to_string(paths::sessions_dir().join(id).join("acp.jsonl")).map_err(|e| e.to_string())?;
                let mut chat = acp::Chat::new(id, "", None);
                for l in text.lines() {
                    let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
                    chat.on_line(v["dir"].as_str().unwrap_or(""), v["line"].as_str().unwrap_or(""), false);
                }
                Ok(json!({ "session": id, "from": 0, "items": chat.items, "state": "ended" }))
            }
            other => Err(format!("agents: unknown method {other}")),
        }
    }

    fn on_ui_event(&self, topic: &str, data: &Value) {
        let Some(id) = data["session"].as_str() else { return };
        let Some(sess) = self.get(id) else { return };
        let mut s = sess.lock().unwrap();
        let bytes = match topic {
            "ui.key" => {
                let Ok(k) = serde_json::from_value::<KeyInput>(data["key"].clone()) else { return };
                s.model.scroll_to_bottom();
                s.dirty = true;
                s.model.key_bytes(&k)
            }
            "ui.paste" => Some(s.model.paste_bytes(data["text"].as_str().unwrap_or(""))),
            "ui.resize" => {
                let cols = data["cols"].as_u64().unwrap_or(80) as u16;
                let rows = data["rows"].as_u64().unwrap_or(24) as u16;
                if s.model.size() != (cols, rows) {
                    s.model.resize(cols, rows);
                    s.dirty = true;
                    if let Some(tx) = &s.shim_tx {
                        let _ = tx.send(json!({ "t": "resize", "cols": cols, "rows": rows }).to_string() + "\n");
                    }
                }
                None
            }
            "ui.scroll" => {
                s.model.scroll(data["delta"].as_i64().unwrap_or(0) as i32);
                s.dirty = true;
                None
            }
            _ => None,
        };
        if let (Some(bytes), Some(tx)) = (bytes, &s.shim_tx) {
            let _ = tx.send(json!({ "t": "input", "b64": b64e(&bytes) }).to_string() + "\n");
        }
    }

    fn send_shim(&self, id: &str, msg: Value) {
        if let Some(sess) = self.get(id) {
            if let Some(tx) = &sess.lock().unwrap().shim_tx {
                let _ = tx.send(msg.to_string() + "\n");
            }
        }
    }

    fn pump(&self) {
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        for sess in sessions {
            let mut s = sess.lock().unwrap();
            if !s.replaying {
                let id = s.info.id.clone();
                if let Some(c) = &mut s.chat {
                    if let Some(from) = c.dirty_from.take() {
                        let from = from.min(c.items.len());
                        self.bus().emit(
                            "chat.items",
                            json!({ "session": id, "from": from, "items": &c.items[from..], "state": c.state }),
                        );
                    }
                    continue;
                }
            }
            if s.dirty && !s.replaying {
                s.dirty = false;
                let frame = s.model.frame();
                self.bus().emit("term.frame", json!({ "session": s.info.id, "frame": frame }));
                if s.model.title != s.last_title {
                    s.last_title = s.model.title.clone();
                    if let Some(t) = &s.last_title {
                        let bus = self.bus().clone();
                        let id = s.info.id.clone();
                        let t = t.clone();
                        tokio::spawn(async move {
                            let _ = bus.request("services", "session.update", json!({ "id": id, "title": t })).await;
                        });
                    }
                }
            }
        }
    }

    async fn create(self: &Arc<Self>, agent_id: &str, cwd: &str, cols: u16, rows: u16) -> Result<SessionInfo, String> {
        let agent = self
            .settings
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .cloned()
            .ok_or_else(|| format!("unknown agent '{agent_id}'"))?;
        let running = self.sessions.lock().unwrap().len();
        if running >= self.settings.max_sessions {
            return Err(format!("session limit ({}) reached", self.settings.max_sessions));
        }
        let id = format!("s{}{}", radix36(now_ms() as u64), self.counter.fetch_add(1, Ordering::Relaxed));
        let (prog, mut args) = resolve_command(&agent)?;

        let vs = VersionState::load();
        let shim = vs.exe("helpers", "tk-shim", false).ok_or("helpers not installed")?;
        let is_chat = agent.kind == "acp";
        if agent.mcp && !is_chat {
            let bridge = vs.exe("helpers", "tk-mcp-bridge", false).ok_or("helpers not installed")?;
            let dir = paths::sessions_dir().join(&id);
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let cfg = json!({ "mcpServers": { "tratschkathl": {
                "type": "stdio",
                "command": bridge,
                "args": ["--session", id],
                "env": { "TK_HOME": paths::home(), "TK_SESSION": id }
            }}});
            let path = dir.join("mcp.json");
            std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap()).map_err(|e| e.to_string())?;
            args.push("--mcp-config".into());
            args.push(path.to_string_lossy().into_owned());
        }

        let mut cmd = std::process::Command::new(&shim);
        cmd.args(["--session", &id, "--cwd", cwd, "--mode", if is_chat { "stdio" } else { "pty" }])
            .args(["--cols", &cols.to_string(), "--rows", &rows.to_string(), "--"])
            .arg(&prog)
            .args(&args)
            .env("TK_HOME", paths::home());
        tk_bus::util::detached_std(&mut cmd);
        cmd.spawn().map_err(|e| format!("starting shim: {e}"))?;
        tracing::info!("session {id}: {prog} {args:?} in {cwd}");

        let info = SessionInfo {
            id: id.clone(),
            kind: if is_chat { "chat".into() } else { "terminal".into() },
            agent: agent.id.clone(),
            title: agent.name.clone(),
            cwd: cwd.to_string(),
            state: "running".into(),
            created_at: now_ms(),
        };
        self.bus()
            .request_timeout("services", "session.register", serde_json::to_value(&info).unwrap(), Duration::from_secs(30))
            .await?;
        self.add_session(info.clone(), cols, rows);
        Ok(info)
    }

    fn add_session(self: &Arc<Self>, info: SessionInfo, cols: u16, rows: u16) {
        let id = info.id.clone();
        let chat = (info.kind == "chat").then(|| {
            let bridge = VersionState::load().exe("helpers", "tk-mcp-bridge", false);
            acp::Chat::new(&info.id, &info.cwd, bridge)
        });
        let sess = Arc::new(Mutex::new(Sess {
            info,
            chat,
            model: TermModel::new(cols, rows),
            shim_tx: None,
            dirty: true,
            replaying: true,
            fresh: false,
            last_title: None,
        }));
        self.sessions.lock().unwrap().insert(id.clone(), sess.clone());
        let me = self.clone();
        tokio::spawn(async move {
            let ended = me.attach(&id, sess).await;
            if let Err(e) = &ended {
                tracing::warn!("session {id}: {e}");
            }
            me.sessions.lock().unwrap().remove(&id);
            let _ = me
                .bus()
                .request_timeout("services", "session.update", json!({ "id": id, "state": "ended" }), Duration::from_secs(30))
                .await;
            me.bus().emit("term.ended", json!({ "session": id }));
        });
    }

    /// Attach to the session's shim and pump its output into the terminal model until the
    /// agent exits or the shim disappears.
    async fn attach(&self, id: &str, sess: Arc<Mutex<Sess>>) -> Result<(), String> {
        let mut stream = None;
        for _ in 0..50 {
            let name = paths::shim_socket(id).to_ns_name::<GenericNamespaced>().map_err(|e| e.to_string())?;
            if let Ok(s) = Stream::connect(name).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let stream = stream.ok_or("shim not reachable")?;
        #[allow(deprecated)]
        let (r, mut w) = stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if w.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let _ = tx.send(json!({ "t": "attach", "from": 0 }).to_string() + "\n");
        sess.lock().unwrap().shim_tx = Some(tx.clone());

        let mut lines = BufReader::new(r).lines();
        let mut result = Err("shim went away".to_string());
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            let mut s = sess.lock().unwrap();
            match msg["t"].as_str() {
                Some("hello") => {
                    s.fresh = msg["fresh"].as_bool().unwrap_or(false);
                    s.replaying = true;
                    let cols = msg["cols"].as_u64().unwrap_or(120) as u16;
                    let rows = msg["rows"].as_u64().unwrap_or(36) as u16;
                    s.model = TermModel::new(cols, rows);
                }
                Some("output") => {
                    let data = b64d(msg["b64"].as_str().unwrap_or(""));
                    let reply = s.model.advance(&data);
                    // Answer terminal queries live, and during the replay of a brand-new
                    // session (ConPTY waits for its cursor query); never answer stale ones.
                    if !reply.is_empty() && (!s.replaying || s.fresh) {
                        let _ = tx.send(json!({ "t": "input", "b64": b64e(&reply) }).to_string() + "\n");
                    }
                    s.dirty = true;
                }
                Some("line") => {
                    let live = !s.replaying;
                    if let Some(chat) = &mut s.chat {
                        let out = chat.on_line(msg["dir"].as_str().unwrap_or(""), msg["line"].as_str().unwrap_or(""), live);
                        for l in out {
                            let _ = tx.send(json!({ "t": "send", "line": l }).to_string() + "\n");
                        }
                    }
                }
                Some("replay_end") => {
                    s.replaying = false;
                    s.dirty = true;
                    if let Some(chat) = &mut s.chat {
                        for l in chat.after_replay() {
                            let _ = tx.send(json!({ "t": "send", "line": l }).to_string() + "\n");
                        }
                    }
                }
                Some("exit") => {
                    tracing::info!("session {id}: agent exited ({})", msg["code"]);
                    let sid = s.info.id.clone();
                    if let Some(chat) = &mut s.chat {
                        chat.ended(msg["code"].as_i64().unwrap_or(-1));
                        let from = chat.dirty_from.take().unwrap_or(0).min(chat.items.len());
                        self.bus().emit(
                            "chat.items",
                            json!({ "session": sid, "from": from, "items": &chat.items[from..], "state": chat.state }),
                        );
                    }
                    result = Ok(());
                    break;
                }
                _ => {}
            }
        }
        writer.abort();
        result
    }
}

fn resolve_command(agent: &AgentConfig) -> Result<(String, Vec<String>), String> {
    let path: PathBuf = which::which(&agent.command).map_err(|_| {
        format!(
            "'{}' was not found on PATH. Edit the agents in {}",
            agent.command,
            Settings::path().display()
        )
    })?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        let mut args = vec!["/c".to_string(), path.to_string_lossy().into_owned()];
        args.extend(agent.args.iter().cloned());
        Ok(("cmd.exe".into(), args))
    } else {
        Ok((path.to_string_lossy().into_owned(), agent.args.clone()))
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn radix36(mut n: u64) -> String {
    const D: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut s = vec![];
    while n > 0 {
        s.push(D[(n % 36) as usize]);
        n /= 36;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

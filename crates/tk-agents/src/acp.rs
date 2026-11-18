//! ACP client side for one chat session (Agent Client Protocol, JSON-RPC 2.0 over the
//! agent's stdio, relayed by tk-shim).
//!
//! The same code path handles live traffic and the replay of the shim's log after an Agent
//! Handler reload: during replay nothing is executed, the transcript is rebuilt and requests
//! from the agent that were never answered are answered afterwards.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::{json, Value};

use tk_bus::model::{ChatItem, ChatOption};

pub struct Chat {
    pub items: Vec<ChatItem>,
    /// `starting` | `idle` | `working` | `waiting` | `error` | `ended`
    pub state: String,
    /// First transcript index the UI has not seen yet.
    pub dirty_from: Option<usize>,
    acp_session: Option<String>,
    next_id: u64,
    /// Our requests to the agent awaiting a response: id → method.
    out_pending: HashMap<u64, String>,
    /// Agent requests to us not answered yet: id (JSON text) → request.
    in_pending: BTreeMap<String, Value>,
    session: String,
    cwd: String,
    bridge: Option<PathBuf>,
    seq: u64,
}

fn line(v: Value) -> String {
    v.to_string()
}

fn response(id: &Value, result: Value) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error_response(id: &Value, code: i64, message: String) -> String {
    line(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

/// Text of a prompt (`[{type:"text", text}]`).
fn prompt_text(prompt: &Value) -> String {
    prompt
        .as_array()
        .map(|a| a.iter().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// Render ACP tool-call content (text, diffs, terminals) as plain text.
fn tool_content_text(content: &Value) -> String {
    const MAX_DIFF_LINES: usize = 80;
    let mut out = vec![];
    for c in content.as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("content") => {
                if let Some(t) = c["content"]["text"].as_str() {
                    out.push(t.to_string());
                }
            }
            Some("diff") => {
                let mut d = vec![c["path"].as_str().unwrap_or("").to_string()];
                if let Some(old) = c["oldText"].as_str() {
                    d.extend(old.lines().map(|l| format!("-{l}")));
                }
                d.extend(c["newText"].as_str().unwrap_or("").lines().map(|l| format!("+{l}")));
                if d.len() > MAX_DIFF_LINES {
                    let more = d.len() - MAX_DIFF_LINES;
                    d.truncate(MAX_DIFF_LINES);
                    d.push(format!("… {more} more lines"));
                }
                out.push(d.join("\n"));
            }
            Some("terminal") => out.push(format!("[terminal {}]", c["terminalId"].as_str().unwrap_or(""))),
            _ => {}
        }
    }
    out.join("\n")
}

impl Chat {
    pub fn new(session: &str, cwd: &str, bridge: Option<PathBuf>) -> Chat {
        Chat {
            items: vec![],
            state: "starting".into(),
            dirty_from: Some(0),
            acp_session: None,
            next_id: 1,
            out_pending: HashMap::new(),
            in_pending: BTreeMap::new(),
            session: session.to_string(),
            cwd: cwd.to_string(),
            bridge,
            seq: 0,
        }
    }

    fn touch(&mut self, index: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(index, |d| d.min(index)));
    }

    fn push(&mut self, mut item: ChatItem) -> usize {
        if item.id.is_empty() {
            self.seq += 1;
            item.id = format!("i{}", self.seq);
        }
        self.items.push(item);
        let ix = self.items.len() - 1;
        self.touch(ix);
        ix
    }

    fn set_state(&mut self, state: &str) {
        if self.state != state {
            self.state = state.to_string();
            let n = self.items.len();
            self.touch(n);
        }
    }

    fn error(&mut self, text: impl Into<String>) {
        self.push(ChatItem { kind: "error".into(), text: text.into(), ..Default::default() });
    }

    fn request(&mut self, method: &str, params: Value) -> String {
        let id = self.next_id;
        self.next_id += 1;
        self.out_pending.insert(id, method.to_string());
        line(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
    }

    /// Open the ACP connection.
    pub fn start(&mut self) -> Vec<String> {
        self.set_state("starting");
        vec![self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true }, "terminal": false },
                "clientInfo": { "name": "tratschkathl", "version": env!("CARGO_PKG_VERSION") }
            }),
        )]
    }

    fn new_session(&mut self) -> String {
        let mut servers = vec![];
        if let Some(bridge) = &self.bridge {
            // Orchestrator tools. M2 uses the MCP bridge (spec §4.1 fallback path).
            servers.push(json!({
                "name": "tratschkathl",
                "command": bridge,
                "args": ["--session", self.session],
                "env": [
                    { "name": "TK_HOME", "value": tk_bus::paths::home() },
                    { "name": "TK_SESSION", "value": self.session }
                ]
            }));
        }
        self.request("session/new", json!({ "cwd": self.cwd, "mcpServers": servers }))
    }

    pub fn prompt(&mut self, text: &str) -> Result<Vec<String>, String> {
        let sid = self.acp_session.clone().ok_or("the agent is not ready yet")?;
        if self.state == "working" || self.state == "waiting" {
            return Err("the agent is still working; cancel first".into());
        }
        self.push(ChatItem { kind: "user".into(), text: text.to_string(), ..Default::default() });
        self.set_state("working");
        Ok(vec![self.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": text }] }))])
    }

    pub fn cancel(&mut self) -> Vec<String> {
        let mut out = vec![];
        // Pending permission requests are answered as cancelled (ACP requirement).
        let ids: Vec<String> = self
            .in_pending
            .iter()
            .filter(|(_, r)| r["method"] == "session/request_permission")
            .map(|(k, _)| k.clone())
            .collect();
        for k in ids {
            out.extend(self.choose(&k, None));
        }
        if let Some(sid) = &self.acp_session {
            out.push(line(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": sid } })));
        }
        out
    }

    /// Answer a permission request with an option id (`None` = cancelled).
    pub fn choose(&mut self, request: &str, option: Option<&str>) -> Vec<String> {
        let Some(req) = self.in_pending.remove(request) else { return vec![] };
        let outcome = match option {
            Some(o) => json!({ "outcome": "selected", "optionId": o }),
            None => json!({ "outcome": "cancelled" }),
        };
        self.resolve_permission(request, option.unwrap_or("cancelled"));
        self.set_state("working");
        vec![response(&req["id"], json!({ "outcome": outcome }))]
    }

    fn resolve_permission(&mut self, request: &str, choice: &str) {
        if let Some(ix) = self.items.iter().position(|i| i.kind == "permission" && i.id == request) {
            self.items[ix].chosen = Some(choice.to_string());
            self.touch(ix);
        }
    }

    /// One line of the conversation. `dir` is `agent` (from the agent) or `client` (what we
    /// sent, only seen during replay). Returns lines to send to the agent (live only).
    pub fn on_line(&mut self, dir: &str, text: &str, live: bool) -> Vec<String> {
        let Ok(msg) = serde_json::from_str::<Value>(text) else { return vec![] };
        let has_method = msg.get("method").is_some();
        let has_id = msg.get("id").is_some_and(|i| !i.is_null());
        match (dir, has_method, has_id) {
            // Our own requests, replayed.
            ("client", true, true) => {
                let id = msg["id"].as_u64().unwrap_or(0);
                let method = msg["method"].as_str().unwrap_or("").to_string();
                self.next_id = self.next_id.max(id + 1);
                if method == "session/prompt" {
                    self.push(ChatItem { kind: "user".into(), text: prompt_text(&msg["params"]["prompt"]), ..Default::default() });
                    self.set_state("working");
                }
                self.out_pending.insert(id, method);
                vec![]
            }
            // Our answers to the agent's requests, replayed.
            ("client", false, true) => {
                let key = msg["id"].to_string();
                if self.in_pending.remove(&key).is_some() {
                    let choice = msg["result"]["outcome"]["optionId"].as_str().unwrap_or("cancelled").to_string();
                    self.resolve_permission(&key, &choice);
                }
                vec![]
            }
            ("client", _, _) => vec![],
            // The agent asks us something.
            ("agent", true, true) => self.on_request(msg, live),
            // Streaming updates.
            ("agent", true, false) => {
                if msg["method"] == "session/update" {
                    self.on_update(&msg["params"]["update"]);
                }
                vec![]
            }
            // Answers to our requests.
            ("agent", false, true) => {
                let id = msg["id"].as_u64().unwrap_or(0);
                match self.out_pending.remove(&id) {
                    Some(method) => self.on_response(&method, &msg, live),
                    None => vec![],
                }
            }
            _ => vec![],
        }
    }

    /// After the replay: answer what is still open, and start the connection if needed.
    pub fn after_replay(&mut self) -> Vec<String> {
        let mut out = vec![];
        let open: Vec<Value> = self.in_pending.values().filter(|r| r["method"] != "session/request_permission").cloned().collect();
        for req in open {
            self.in_pending.remove(&req["id"].to_string());
            out.extend(self.on_request(req, true));
        }
        if self.acp_session.is_none() && self.out_pending.is_empty() && self.state != "error" {
            out.extend(self.start());
        }
        out
    }

    pub fn ended(&mut self, code: i64) {
        self.push(ChatItem { kind: "info".into(), text: format!("Agent process exited ({code})."), ..Default::default() });
        self.set_state("ended");
    }

    fn on_request(&mut self, req: Value, live: bool) -> Vec<String> {
        let id = req["id"].clone();
        let key = id.to_string();
        let method = req["method"].as_str().unwrap_or("").to_string();
        let p = &req["params"];
        if method == "session/request_permission" {
            let options = p["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| ChatOption {
                    id: o["optionId"].as_str().unwrap_or("").into(),
                    name: o["name"].as_str().unwrap_or("").into(),
                    kind: o["kind"].as_str().unwrap_or("").into(),
                })
                .collect();
            let title = p["toolCall"]["title"].as_str().map(String::from).or_else(|| Some("Permission requested".into()));
            self.push(ChatItem {
                id: key.clone(),
                kind: "permission".into(),
                title,
                text: tool_content_text(&p["toolCall"]["content"]),
                options,
                ..Default::default()
            });
            self.in_pending.insert(key, req);
            self.set_state("waiting");
            return vec![];
        }
        if !live {
            self.in_pending.insert(key, req);
            return vec![];
        }
        match method.as_str() {
            "fs/read_text_file" => {
                let path = p["path"].as_str().unwrap_or("");
                match std::fs::read_to_string(path) {
                    Ok(text) => {
                        let start = p["line"].as_u64().map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
                        let limit = p["limit"].as_u64().map(|l| l as usize).unwrap_or(usize::MAX);
                        let content = if start == 0 && limit == usize::MAX {
                            text
                        } else {
                            text.lines().skip(start).take(limit).collect::<Vec<_>>().join("\n")
                        };
                        vec![response(&id, json!({ "content": content }))]
                    }
                    Err(e) => vec![error_response(&id, -32603, format!("{path}: {e}"))],
                }
            }
            "fs/write_text_file" => {
                let path = PathBuf::from(p["path"].as_str().unwrap_or(""));
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::write(&path, p["content"].as_str().unwrap_or("")) {
                    Ok(()) => vec![response(&id, Value::Null)],
                    Err(e) => vec![error_response(&id, -32603, format!("{}: {e}", path.display()))],
                }
            }
            other => vec![error_response(&id, -32601, format!("method not supported by Tratschkathl: {other}"))],
        }
    }

    fn on_response(&mut self, method: &str, msg: &Value, live: bool) -> Vec<String> {
        if let Some(err) = msg.get("error") {
            let text = err["message"].as_str().unwrap_or("error").to_string();
            match method {
                "initialize" | "session/new" => {
                    self.error(format!("{method} failed: {text}"));
                    self.set_state("error");
                }
                _ => {
                    self.error(text);
                    self.set_state("idle");
                }
            }
            return vec![];
        }
        let r = &msg["result"];
        match method {
            "initialize" => {
                if live {
                    vec![self.new_session()]
                } else {
                    vec![]
                }
            }
            "session/new" => {
                self.acp_session = r["sessionId"].as_str().map(String::from);
                self.set_state("idle");
                vec![]
            }
            "session/prompt" => {
                let reason = r["stopReason"].as_str().unwrap_or("end_turn");
                if reason != "end_turn" {
                    self.push(ChatItem { kind: "info".into(), text: format!("Turn ended: {reason}"), ..Default::default() });
                }
                self.set_state("idle");
                vec![]
            }
            _ => vec![],
        }
    }

    fn on_update(&mut self, u: &Value) {
        let kind = u["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" | "user_message_chunk" => {
                let item_kind = match kind {
                    "agent_message_chunk" => "agent",
                    "agent_thought_chunk" => "thought",
                    _ => "user",
                };
                let Some(text) = u["content"]["text"].as_str() else { return };
                match self.items.last_mut() {
                    Some(last) if last.kind == item_kind => {
                        last.text.push_str(text);
                        let ix = self.items.len() - 1;
                        self.touch(ix);
                    }
                    _ => {
                        self.push(ChatItem { kind: item_kind.into(), text: text.to_string(), ..Default::default() });
                    }
                }
            }
            "tool_call" => {
                let id = u["toolCallId"].as_str().unwrap_or("").to_string();
                let item = ChatItem {
                    id: format!("tool:{id}"),
                    kind: "tool".into(),
                    title: u["title"].as_str().map(String::from),
                    status: u["status"].as_str().map(String::from).or(Some("pending".into())),
                    text: tool_content_text(&u["content"]),
                    ..Default::default()
                };
                match self.items.iter().position(|i| i.id == item.id) {
                    Some(ix) => {
                        self.items[ix] = item;
                        self.touch(ix);
                    }
                    None => {
                        self.push(item);
                    }
                }
            }
            "tool_call_update" => {
                let id = format!("tool:{}", u["toolCallId"].as_str().unwrap_or(""));
                let ix = match self.items.iter().position(|i| i.id == id) {
                    Some(ix) => ix,
                    None => self.push(ChatItem { id, kind: "tool".into(), ..Default::default() }),
                };
                let item = &mut self.items[ix];
                if let Some(t) = u["title"].as_str() {
                    item.title = Some(t.to_string());
                }
                if let Some(s) = u["status"].as_str() {
                    item.status = Some(s.to_string());
                }
                if u.get("content").is_some_and(|c| c.is_array()) {
                    item.text = tool_content_text(&u["content"]);
                }
                self.touch(ix);
            }
            "plan" => {
                let text = u["entries"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|e| {
                        let mark = match e["status"].as_str() {
                            Some("completed") => "☑",
                            Some("in_progress") => "▶",
                            _ => "☐",
                        };
                        format!("{mark} {}", e["content"].as_str().unwrap_or(""))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                match self.items.iter().position(|i| i.kind == "plan") {
                    Some(ix) => {
                        self.items[ix].text = text;
                        self.touch(ix);
                    }
                    None => {
                        self.push(ChatItem { id: "plan".into(), kind: "plan".into(), title: Some("Plan".into()), text, ..Default::default() });
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(chat: &mut Chat, v: Value, live: bool) -> Vec<String> {
        chat.on_line("agent", &v.to_string(), live)
    }

    #[test]
    fn handshake_prompt_and_permission() {
        let mut c = Chat::new("s1", "C:/x", None);
        let out = c.after_replay();
        assert!(out[0].contains("\"initialize\""));
        let out = agent(&mut c, json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}), true);
        assert!(out[0].contains("session/new"));
        agent(&mut c, json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"acp-1"}}), true);
        assert_eq!(c.state, "idle");
        let out = c.prompt("hello").unwrap();
        assert!(out[0].contains("session/prompt"));
        agent(&mut c, json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hi "}}}}), true);
        agent(&mut c, json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"there"}}}}), true);
        assert_eq!(c.items.last().unwrap().text, "Hi there");
        agent(&mut c, json!({"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"acp-1","toolCall":{"toolCallId":"t1","title":"Edit file"},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"}]}}), true);
        assert_eq!(c.state, "waiting");
        let out = c.choose("\"p1\"", Some("ok"));
        assert!(out[0].contains("\"optionId\":\"ok\""));
        agent(&mut c, json!({"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}), true);
        assert_eq!(c.state, "idle");
    }

    #[test]
    fn replay_rebuilds_and_answers_open_requests() {
        let mut c = Chat::new("s1", "C:/x", None);
        let log = [
            ("client", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})),
            ("agent", json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}})),
            ("client", json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{}})),
            ("agent", json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"acp-1"}})),
            ("client", json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"read it"}]}})),
            ("agent", json!({"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{"path":file!()}})),
        ];
        for (dir, v) in log {
            assert!(c.on_line(dir, &v.to_string(), false).is_empty());
        }
        assert_eq!(c.items[0].text, "read it");
        let out = c.after_replay();
        assert_eq!(out.len(), 1, "only the unanswered fs read is answered: {out:?}");
        assert!(out[0].contains("\"id\":7"));
        // Our next request id continues after the replayed ones.
        assert_eq!(c.state, "working");
        agent(&mut c, json!({"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}), true);
        assert!(c.prompt("next").unwrap()[0].contains("\"id\":4"));
    }
}

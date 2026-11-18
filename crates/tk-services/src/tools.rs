//! Tool registry exposed to agents (MCP for terminal agents, ACP tools later). `host_*`
//! tools are implemented by Core; Services owns their definitions.

use serde_json::{json, Value};

use tk_bus::model::FactInput;

use crate::{facts, Services};

pub fn defs() -> Value {
    json!([
        {
            "name": "fact_announce",
            "description": "Announce a fact about your session: what it is about. Facts group and connect sessions in Tratschkathl's views. Well-known keys: project, branch, issue (e.g. PROJ-123), phase (concept | designed | implementing | completed), topic. Single-valued keys (project, branch, phase, ...) replace the old value; issue, topic, review, build, changelist, doc may hold several. Git facts (project, repo, branch, worktree) are derived automatically; announcing one yourself overrides the derived value. Announce facts as soon as you know them and when they change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Lowercase key, e.g. issue" },
                    "value": { "type": "string" },
                    "kind": { "type": "string", "description": "Optional reference kind, e.g. jira or git.branch, when the value is an id in another system." },
                    "note": { "type": "string", "description": "Optional short note on why." },
                    "session": { "type": "string", "description": "Another session's id. Default: your own session." }
                },
                "required": ["key", "value"]
            }
        },
        {
            "name": "fact_clear",
            "description": "Clear a fact that is no longer true for your session: one value, or every value of the key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string" },
                    "value": { "type": "string", "description": "Clear only this value. Default: all values of the key." },
                    "session": { "type": "string" }
                },
                "required": ["key"]
            }
        },
        {
            "name": "fact_list",
            "description": "List the current facts of a session (default: your own), with their source (agent, user, plugin:git, ...).",
            "inputSchema": { "type": "object", "properties": { "session": { "type": "string" } } }
        },
        {
            "name": "fact_query",
            "description": "Find sessions that share facts with a filter, e.g. {\"issue\": \"PROJ-123\"} or {\"branch\": \"\"} (any value). Returns the matching sessions with their facts. Use it to find related work before starting.",
            "inputSchema": {
                "type": "object",
                "properties": { "filter": { "type": "object", "additionalProperties": { "type": "string" } } },
                "required": ["filter"]
            }
        },
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
            "name": "document_list",
            "description": "List the documents Tratschkathl offers for a session's facts (default: your own session): e.g. the worktree's uncommitted changes (changeset) and its Markdown files. Announce a `doc` fact with a file path to add a specific Markdown file.",
            "inputSchema": { "type": "object", "properties": { "session": { "type": "string" } } }
        },
        {
            "name": "document_open",
            "description": "Open a document in the user's workbench, e.g. to show them the changes you made or a design doc you wrote. Takes a uri from document_list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string" },
                    "placement": { "type": "string", "enum": ["tab", "split"], "description": "Default: a tab in the active pane." }
                },
                "required": ["uri"]
            }
        },
        {
            "name": "session_focus",
            "description": "Bring a session to the front of the user's workbench (opens or activates its tab). Without a session id it focuses your own session.",
            "inputSchema": { "type": "object", "properties": { "session": { "type": "string" } } }
        },
        {
            "name": "session_list",
            "description": "List agent sessions known to Tratschkathl, each with its current facts.",
            "inputSchema": { "type": "object", "properties": { "state": { "type": "string", "enum": ["running", "ended"] } } }
        }
    ])
}

/// `params` = { session, name, args, idem } as forwarded by Core.
pub fn call(svc: &Services, params: &Value) -> Result<Value, String> {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["args"];
    let caller = params["session"].as_str().unwrap_or("");
    let target = args["session"].as_str().filter(|s| !s.is_empty()).unwrap_or(caller).to_string();
    let source = if caller == "ctl" || caller.is_empty() { "user" } else { "agent" };

    let ensure_session = |id: &str| -> Result<(), String> {
        if svc.sessions(None)?.iter().any(|s| s.id == id) {
            Ok(())
        } else {
            Err(format!("unknown session '{id}' (pass session explicitly when not calling from a session)"))
        }
    };

    match name {
        "fact_announce" => {
            ensure_session(&target)?;
            let f = FactInput {
                key: args["key"].as_str().unwrap_or("").to_string(),
                value: args["value"].as_str().unwrap_or("").to_string(),
                kind: args["kind"].as_str().filter(|k| !k.is_empty()).map(String::from),
            };
            let changed = facts::announce(&svc.db.lock().unwrap(), &target, &f, source, args["note"].as_str())?;
            if changed {
                svc.facts_changed(&target);
            }
            Ok(json!({ "session": target, "changed": changed, "multi_valued": facts::is_multi(&f.key) }))
        }
        "fact_clear" => {
            ensure_session(&target)?;
            let key = args["key"].as_str().ok_or("key missing")?;
            let n = facts::clear(&svc.db.lock().unwrap(), &target, key, args["value"].as_str())?;
            if n > 0 {
                svc.facts_changed(&target);
            }
            Ok(json!({ "session": target, "cleared": n }))
        }
        "fact_list" => {
            ensure_session(&target)?;
            let list = facts::current(&svc.db.lock().unwrap(), Some(&target), None).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(list).unwrap())
        }
        "fact_query" => {
            let filter: Vec<FactInput> = args["filter"]
                .as_object()
                .ok_or("filter must be an object of key: value")?
                .iter()
                .map(|(k, v)| FactInput { key: k.clone(), value: v.as_str().unwrap_or("").to_string(), kind: None })
                .collect();
            let ids = facts::query(&svc.db.lock().unwrap(), &filter)?;
            sessions_with_facts(svc, |id| ids.iter().any(|i| i == id), None)
        }
        "session_list" => sessions_with_facts(svc, |_| true, args["state"].as_str()),
        "document_list" => {
            ensure_session(&target)?;
            Ok(serde_json::to_value(svc.documents_for(&target)?).unwrap())
        }
        "document_open" => {
            let uri = args["uri"].as_str().ok_or("uri missing")?;
            tk_bus::model::parse_doc_uri(uri).ok_or("uri must come from document_list")?;
            svc.emit("workbench.open_document", json!({ "uri": uri, "placement": args["placement"].as_str().unwrap_or("tab") }));
            Ok(json!({ "opened": uri }))
        }
        "session_focus" => {
            ensure_session(&target)?;
            svc.emit("workbench.focus", json!({ "session": target }));
            Ok(json!({ "focused": target }))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn sessions_with_facts(svc: &Services, keep: impl Fn(&str) -> bool, state: Option<&str>) -> Result<Value, String> {
    let sessions = svc.sessions(state)?;
    let all = facts::current(&svc.db.lock().unwrap(), None, None).map_err(|e| e.to_string())?;
    Ok(Value::Array(
        sessions
            .into_iter()
            .filter(|s| keep(&s.id))
            .map(|s| {
                let f: serde_json::Map<String, Value> = all
                    .iter()
                    .filter(|f| f.session == s.id)
                    .fold(serde_json::Map::new(), |mut m, f| {
                        match m.get_mut(&f.key) {
                            Some(Value::Array(a)) => a.push(json!(f.value)),
                            Some(v) => *v = json!([v.clone(), f.value]),
                            None => {
                                m.insert(f.key.clone(), json!(f.value));
                            }
                        }
                        m
                    });
                json!({ "id": s.id, "agent": s.agent, "title": s.title, "cwd": s.cwd, "state": s.state, "facts": f })
            })
            .collect(),
    ))
}

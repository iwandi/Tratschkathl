//! tk-mcp-bridge: stdio MCP server started by a terminal agent (Claude Code `--mcp-config`).
//! Handles the MCP handshake itself and forwards tool calls over the bus. Calls are held and
//! re-sent while Core or Services restart, so the agent sees a slow call, never a failed one.
//!
//! Usage: `tk-mcp-bridge --session <id>`

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use tk_bus::Bus;

const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[tokio::main]
async fn main() {
    let mut session = String::from("unknown");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--self-test" => {
                println!("{}", tk_bus::self_test_json("helpers"));
                return;
            }
            "--session" => session = it.next().unwrap_or_default(),
            _ => {}
        }
    }
    if let Ok(s) = std::env::var("TK_SESSION") {
        if session == "unknown" {
            session = s;
        }
    }
    tk_bus::logging::init(&format!("bridge-{session}"));

    let (bus, mut events) = Bus::connect(format!("bridge:{session}"), env!("CARGO_PKG_VERSION"), vec!["layer.up".into()], None);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();

    // stdout writer
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = out_rx.recv().await {
            let line = v.to_string() + "\n";
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    // Tell the agent the tool list may have changed whenever Services comes back.
    {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            while let Some((topic, data)) = events.recv().await {
                if topic == "layer.up" && data["role"] == "services" {
                    let _ = out_tx.send(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
                }
            }
        });
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut seq: u64 = 0;
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            tracing::warn!("bad MCP line");
            continue;
        };
        let Some(id) = msg.get("id").cloned() else { continue }; // notification
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        seq += 1;
        let idem = format!("{session}-{}-{seq}", std::process::id());
        let bus = bus.clone();
        let out_tx = out_tx.clone();
        let session = session.clone();
        tokio::spawn(async move {
            let reply = match method.as_str() {
                "initialize" => Ok(json!({
                    "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "tratschkathl", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Tratschkathl orchestrator tools. host_reload rebuilds and reloads the app you are running in; your session survives the reload."
                })),
                "ping" => Ok(json!({})),
                "tools/list" => match bus.request_timeout("services", "tools.list", json!({}), CALL_TIMEOUT).await {
                    Ok(tools) => Ok(json!({ "tools": tools })),
                    Err(e) => Err((-32603, e)),
                },
                "tools/call" => {
                    let name = params["name"].as_str().unwrap_or("").to_string();
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    let req = json!({ "session": session, "name": name, "args": args, "idem": idem });
                    match bus.request_timeout("core", "tool.call", req, CALL_TIMEOUT).await {
                        Ok(v) => Ok(json!({ "content": [{ "type": "text", "text": pretty(&v) }], "isError": false })),
                        Err(e) => Ok(json!({ "content": [{ "type": "text", "text": e }], "isError": true })),
                    }
                }
                other => Err((-32601, format!("method not found: {other}"))),
            };
            let resp = match reply {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
            };
            let _ = out_tx.send(resp);
        });
    }
    // stdin closed: the agent is gone.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

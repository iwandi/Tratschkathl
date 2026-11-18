//! stdio mode (ACP agents): the agent speaks newline-delimited JSON-RPC on stdin/stdout.
//! Every line in both directions is appended to `sessions/<id>/acp.jsonl` and kept in memory,
//! so a reloaded Agent Handler can replay the whole conversation (spec §3.2: full log for ACP).
//!
//! Socket protocol, client → shim: {"t":"attach","from":N}  {"t":"send","line":"..."}  {"t":"kill"}
//! shim → client: {"t":"hello","fresh":bool,"mode":"stdio"}
//!                {"t":"line","seq":N,"dir":"agent"|"client","line":"..."}  {"t":"replay_end"}
//!                {"t":"exit","code":N}

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
use tokio::sync::mpsc;

use crate::Args;

struct Shared {
    entries: Vec<(String, String)>,
    clients: Vec<mpsc::UnboundedSender<String>>,
    exit: Option<i32>,
    attached_once: bool,
    log: Option<std::fs::File>,
}

impl Shared {
    fn record(&mut self, dir: &str, line: String) {
        let seq = self.entries.len();
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{}", json!({ "dir": dir, "line": line }));
        }
        if dir == "agent" {
            let msg = json!({ "t": "line", "seq": seq, "dir": dir, "line": line }).to_string() + "\n";
            self.clients.retain(|c| c.send(msg.clone()).is_ok());
        }
        self.entries.push((dir.to_string(), line));
    }
}

pub async fn run(args: Args) -> Result<()> {
    let dir = tk_bus::paths::sessions_dir().join(&args.session);
    std::fs::create_dir_all(&dir)?;
    let stderr = std::fs::File::create(dir.join("agent.stderr.log"))?;

    let mut cmd = Command::new(&args.cmd[0]);
    cmd.args(&args.cmd[1..])
        .current_dir(&args.cwd)
        .env("TK_SESSION", &args.session)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(tk_bus::util::CREATE_NO_WINDOW);
    }
    let mut child: Child = cmd.spawn().with_context(|| format!("spawn {:?}", args.cmd))?;
    let stdin: Arc<Mutex<ChildStdin>> = Arc::new(Mutex::new(child.stdin.take().context("stdin")?));
    let stdout = child.stdout.take().context("stdout")?;
    let child = Arc::new(Mutex::new(child));

    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("acp.jsonl")).ok();
    let shared = Arc::new(Mutex::new(Shared { entries: vec![], clients: vec![], exit: None, attached_once: false, log }));

    // Agent stdout → log → clients.
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                shared.lock().unwrap().record("agent", line);
            }
        });
    }
    // Agent exit.
    {
        let shared = shared.clone();
        let child = child.clone();
        std::thread::spawn(move || loop {
            let status = child.lock().unwrap().try_wait();
            match status {
                Ok(Some(s)) => {
                    let code = s.code().unwrap_or(-1);
                    tracing::info!("agent exited with {code}");
                    let mut sh = shared.lock().unwrap();
                    sh.exit = Some(code);
                    let msg = json!({ "t": "exit", "code": code }).to_string() + "\n";
                    sh.clients.retain(|c| c.send(msg.clone()).is_ok());
                    drop(sh);
                    std::thread::sleep(Duration::from_secs(15));
                    std::process::exit(0);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(_) => std::process::exit(1),
            }
        });
    }

    let name = tk_bus::paths::shim_socket(&args.session).to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new().name(name).create_tokio().context("listen")?;
    loop {
        let conn = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("accept: {e}");
                continue;
            }
        };
        let shared = shared.clone();
        let stdin = stdin.clone();
        let child = child.clone();
        tokio::spawn(async move {
            #[allow(deprecated)]
            let (r, mut w) = conn.split();
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let wtask = tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
            let mut lines = TokioBufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match msg["t"].as_str() {
                    Some("attach") => {
                        let from = msg["from"].as_u64().unwrap_or(0) as usize;
                        let mut sh = shared.lock().unwrap();
                        let fresh = !sh.attached_once;
                        sh.attached_once = true;
                        let _ = tx.send(json!({ "t": "hello", "fresh": fresh, "mode": "stdio" }).to_string() + "\n");
                        for (seq, (dir, l)) in sh.entries.iter().enumerate().skip(from) {
                            let _ = tx.send(json!({ "t": "line", "seq": seq, "dir": dir, "line": l }).to_string() + "\n");
                        }
                        let _ = tx.send(json!({ "t": "replay_end" }).to_string() + "\n");
                        if let Some(code) = sh.exit {
                            let _ = tx.send(json!({ "t": "exit", "code": code }).to_string() + "\n");
                        }
                        sh.clients.push(tx.clone());
                    }
                    Some("send") => {
                        let Some(l) = msg["line"].as_str() else { continue };
                        let ok = {
                            let mut w = stdin.lock().unwrap();
                            w.write_all(l.as_bytes()).and_then(|_| w.write_all(b"\n")).and_then(|_| w.flush()).is_ok()
                        };
                        if ok {
                            shared.lock().unwrap().record("client", l.to_string());
                        }
                    }
                    Some("kill") => {
                        let _ = child.lock().unwrap().kill();
                    }
                    _ => {}
                }
            }
            wtask.abort();
        });
    }
}

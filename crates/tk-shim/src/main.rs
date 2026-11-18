//! tk-shim: one per agent session (like dtach). Owns the PTY and the agent process, records
//! the output in a ring buffer and serves it from any offset over a local socket, so every
//! layer (Core included) can restart without killing the agent.
//!
//! Usage: `tk-shim --session <id> --cwd <dir> [--cols N --rows N] -- <command> [args...]`
//!
//! Socket protocol (JSON lines), client → shim:
//!   {"t":"attach","from":0}  {"t":"input","b64":"..."}  {"t":"resize","cols":..,"rows":..}  {"t":"kill"}
//! shim → client:
//!   {"t":"hello","fresh":bool,"start":N,"cols":N,"rows":N}  {"t":"output","offset":N,"b64":"..."}  {"t":"replay_end"}
//!   {"t":"exit","code":N}

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

const RING: usize = 4 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

struct Shared {
    buf: VecDeque<u8>,
    /// Absolute offset of `buf[0]`.
    start: u64,
    clients: Vec<mpsc::UnboundedSender<String>>,
    exit: Option<i32>,
    attached_once: bool,
    size: (u16, u16),
}

impl Shared {
    fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }
    fn broadcast(&mut self, line: String) {
        self.clients.retain(|c| c.send(line.clone()).is_ok());
    }
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

struct Args {
    session: String,
    cwd: String,
    cols: u16,
    rows: u16,
    cmd: Vec<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args { session: String::new(), cwd: ".".into(), cols: 120, rows: 36, cmd: vec![] };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--self-test" => {
                println!("{}", tk_bus::self_test_json("helpers"));
                std::process::exit(0);
            }
            "--session" => a.session = it.next().context("--session value")?,
            "--cwd" => a.cwd = it.next().context("--cwd value")?,
            "--cols" => a.cols = it.next().context("--cols value")?.parse()?,
            "--rows" => a.rows = it.next().context("--rows value")?.parse()?,
            "--" => {
                a.cmd = it.by_ref().collect();
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(!a.session.is_empty() && !a.cmd.is_empty(), "need --session and a command");
    Ok(a)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    tk_bus::logging::init(&format!("shim-{}", args.session));
    tracing::info!("shim {} starting: {:?} in {}", args.session, args.cmd, args.cwd);

    let pty = native_pty_system()
        .openpty(PtySize { rows: args.rows, cols: args.cols, pixel_width: 0, pixel_height: 0 })
        .context("openpty")?;
    let mut cb = CommandBuilder::new(&args.cmd[0]);
    cb.args(&args.cmd[1..]);
    cb.cwd(&args.cwd);
    cb.env("TK_SESSION", &args.session);
    cb.env("TERM", "xterm-256color");
    cb.env("COLORTERM", "truecolor");
    let mut child = pty.slave.spawn_command(cb).context("spawn agent")?;
    drop(pty.slave);
    let killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>> = Arc::new(Mutex::new(child.clone_killer()));
    let mut reader = pty.master.try_clone_reader()?;
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pty.master.take_writer()?));
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pty.master));

    let shared = Arc::new(Mutex::new(Shared {
        buf: VecDeque::with_capacity(64 * 1024),
        start: 0,
        clients: vec![],
        exit: None,
        attached_once: false,
        size: (args.cols, args.rows),
    }));

    // PTY output → ring buffer → attached clients.
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; CHUNK];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut sh = shared.lock().unwrap();
                        let offset = sh.end();
                        sh.buf.extend(&chunk[..n]);
                        let over = sh.buf.len().saturating_sub(RING);
                        if over > 0 {
                            sh.buf.drain(..over);
                            sh.start += over as u64;
                        }
                        let line = json!({ "t": "output", "offset": offset, "b64": b64(&chunk[..n]) }).to_string() + "\n";
                        sh.broadcast(line);
                    }
                }
            }
        });
    }

    // Child exit → tell clients, linger a little so a reloading Agent Handler can still see it.
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            tracing::info!("agent exited with {code}");
            {
                let mut sh = shared.lock().unwrap();
                sh.exit = Some(code);
                sh.broadcast(json!({ "t": "exit", "code": code }).to_string() + "\n");
            }
            std::thread::sleep(Duration::from_secs(15));
            std::process::exit(0);
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
        let writer = writer.clone();
        let master = master.clone();
        let killer = killer.clone();
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
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match msg["t"].as_str() {
                    Some("attach") => {
                        let from = msg["from"].as_u64().unwrap_or(0);
                        let mut sh = shared.lock().unwrap();
                        let fresh = !sh.attached_once;
                        sh.attached_once = true;
                        let _ = tx.send(json!({ "t": "hello", "fresh": fresh, "start": sh.start, "cols": sh.size.0, "rows": sh.size.1 }).to_string() + "\n");
                        let skip = from.saturating_sub(sh.start) as usize;
                        let (a, b) = sh.buf.as_slices();
                        let data: Vec<u8> = a.iter().chain(b.iter()).skip(skip).copied().collect();
                        let mut offset = sh.start + skip as u64;
                        for c in data.chunks(CHUNK) {
                            let _ = tx.send(json!({ "t": "output", "offset": offset, "b64": b64(c) }).to_string() + "\n");
                            offset += c.len() as u64;
                        }
                        let _ = tx.send(json!({ "t": "replay_end" }).to_string() + "\n");
                        if let Some(code) = sh.exit {
                            let _ = tx.send(json!({ "t": "exit", "code": code }).to_string() + "\n");
                        }
                        sh.clients.push(tx.clone());
                    }
                    Some("input") => {
                        if let Some(data) = msg["b64"].as_str().and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok()) {
                            let mut w = writer.lock().unwrap();
                            let _ = w.write_all(&data);
                            let _ = w.flush();
                        }
                    }
                    Some("resize") => {
                        let cols = msg["cols"].as_u64().unwrap_or(80) as u16;
                        let rows = msg["rows"].as_u64().unwrap_or(24) as u16;
                        shared.lock().unwrap().size = (cols, rows);
                        let _ = master.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                    }
                    Some("kill") => {
                        let _ = killer.lock().unwrap().kill();
                    }
                    _ => {}
                }
            }
            wtask.abort();
        });
    }
}

//! L1 Core: bus hub, layer supervisor, builds and reloads, handoff store.

mod decisions;
mod reload;
mod router;
mod supervisor;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use tk_bus::paths;
use tk_bus::versions::{VersionState, LAYERS};

use reload::Reloader;
use router::Router;
use supervisor::LayerHandle;

struct CoreApp {
    router: Arc<Router>,
    reloader: Arc<Reloader>,
    layers: HashMap<&'static str, LayerHandle>,
    exit_tx: mpsc::UnboundedSender<i32>,
    safe_mode: bool,
}

static APP: OnceLock<Arc<CoreApp>> = OnceLock::new();

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--self-test") {
        println!("{}", tk_bus::self_test_json("core"));
        return;
    }
    let safe_mode = args.iter().any(|a| a == "--safe-mode");
    let headless = args.iter().any(|a| a == "--headless") || std::env::var("TK_HEADLESS").is_ok();
    tk_bus::logging::init("core");
    if let Err(e) = paths::ensure_dirs() {
        eprintln!("cannot create dirs: {e}");
        std::process::exit(1);
    }
    tracing::info!("core starting pid={} safe_mode={safe_mode} headless={headless}", std::process::id());

    let handler = tk_bus::client::handler(|inc| async move {
        let app = loop {
            if let Some(a) = APP.get() {
                break a.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        app.handle(inc).await
    });
    let router = Router::new(LAYERS, handler);

    let name = paths::core_socket().to_ns_name::<GenericNamespaced>().expect("socket name");
    let listener = match ListenerOptions::new().name(name).create_tokio() {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot listen on {} ({e}); another Core running?", paths::core_socket());
            std::process::exit(1);
        }
    };
    {
        let router = router.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(conn) => {
                        tokio::spawn(router.clone().serve_conn(conn));
                    }
                    Err(e) => tracing::warn!("accept failed: {e}"),
                }
            }
        });
    }

    let mut layers = HashMap::new();
    for unit in LAYERS {
        if headless && *unit == "ui" {
            continue;
        }
        layers.insert(*unit, supervisor::spawn(router.clone(), unit, safe_mode));
    }
    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<i32>();
    let reloader = Arc::new(Reloader::new(router.clone(), layers.clone(), exit_tx.clone()));
    let app = Arc::new(CoreApp { router: router.clone(), reloader: reloader.clone(), layers, exit_tx, safe_mode });
    let _ = APP.set(app);

    let _ = tk_bus::util::write_atomic(
        &paths::state_dir().join("core.ready"),
        std::process::id().to_string().as_bytes(),
    );
    {
        let router = router.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                router.expire(Duration::from_secs(600));
            }
        });
    }
    tokio::spawn(reloader.resume());

    let code = exit_rx.recv().await.unwrap_or(0);
    tracing::info!("core exiting with {code}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::process::exit(code);
}

impl CoreApp {
    async fn handle(&self, inc: tk_bus::Incoming) -> Result<Value, String> {
        match inc.method.as_str() {
            "tool.call" => {
                let name = inc.params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = inc.params.get("args").cloned().unwrap_or(json!({}));
                if name.starts_with("host_") {
                    let idem = inc
                        .params
                        .get("idem")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .unwrap_or_else(|| format!("job-{}", tk_bus::model::now_ms()));
                    self.host_tool(&name, &idem, &args).await
                } else {
                    self.router.call("services", "tool.call", inc.params, Duration::from_secs(600)).await
                }
            }
            "handoff.put" => {
                let unit = inc.params["unit"].as_str().ok_or("unit missing")?;
                let blob = serde_json::to_vec(&inc.params["blob"]).map_err(|e| e.to_string())?;
                tk_bus::util::write_atomic(&paths::handoff_dir().join(format!("{unit}.json")), &blob)
                    .map_err(|e| e.to_string())?;
                Ok(json!(true))
            }
            "handoff.take" => {
                let unit = inc.params["unit"].as_str().ok_or("unit missing")?;
                let path = paths::handoff_dir().join(format!("{unit}.json"));
                let blob = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                let _ = std::fs::remove_file(&path);
                Ok(blob.unwrap_or(Value::Null))
            }
            "app.quit" => {
                let router = self.router.clone();
                let layers = self.layers.clone();
                let exit_tx = self.exit_tx.clone();
                tokio::spawn(async move {
                    tracing::info!("quit requested");
                    let _ = router.call("agents", "agents.kill_all", json!({}), Duration::from_secs(10)).await;
                    for unit in ["ui", "agents", "services"] {
                        if let Some(h) = layers.get(unit) {
                            h.stop().await;
                        }
                    }
                    let _ = exit_tx.send(0);
                });
                Ok(json!(true))
            }
            "host.status" => self.host_tool("host_status", "", &json!({})).await,
            other => Err(format!("core: unknown method {other}")),
        }
    }

    async fn host_tool(&self, name: &str, idem: &str, args: &Value) -> Result<Value, String> {
        match name {
            "host_status" => {
                let vs = VersionState::load();
                Ok(json!({
                    "core": { "pid": std::process::id(), "safe_mode": self.safe_mode, "proto": [tk_bus::PROTO.0, tk_bus::PROTO.1] },
                    "versions": vs.units,
                    "source_dir": vs.source_dir,
                    "connected": self.router.connected_units(),
                }))
            }
            "host_reload" => self.reloader.host_reload(idem, args).await,
            "host_rollback" => self.reloader.host_rollback(args).await,
            other => Err(format!("unknown host tool {other}")),
        }
    }
}

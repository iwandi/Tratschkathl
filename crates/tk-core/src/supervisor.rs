//! One supervisor task per layer process (services, agents, ui): start it, adopt it after a
//! Core restart, restart it after a crash, roll back after repeated crashes, and swap it to a
//! new version at a safe point.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use tk_bus::versions::VersionState;

use crate::router::Router;

pub enum LayerCmd {
    Swap { to: String, reply: oneshot::Sender<Result<(), String>> },
    Stop { reply: oneshot::Sender<()> },
}

#[derive(Clone)]
pub struct LayerHandle {
    tx: mpsc::UnboundedSender<LayerCmd>,
}

impl LayerHandle {
    pub async fn swap(&self, to: &str) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(LayerCmd::Swap { to: to.to_string(), reply }).map_err(|_| "supervisor gone".to_string())?;
        rx.await.map_err(|_| "supervisor gone".to_string())?
    }

    pub async fn stop(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(LayerCmd::Stop { reply }).is_ok() {
            let _ = rx.await;
        }
    }
}

const READY_TIMEOUT: Duration = Duration::from_secs(45);
const STABLE_FOR: Duration = Duration::from_secs(3);
const PROMOTE_AFTER: Duration = Duration::from_secs(60);

pub fn spawn(router: Arc<Router>, unit: &'static str, safe_mode: bool) -> LayerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(Supervisor { router, unit, safe_mode, crashes: vec![], started: None }.run(rx));
    LayerHandle { tx }
}

struct Supervisor {
    router: Arc<Router>,
    unit: &'static str,
    safe_mode: bool,
    crashes: Vec<Instant>,
    started: Option<Instant>,
}

enum Outcome {
    Down,
    Cmd(LayerCmd),
}

impl Supervisor {
    fn present(&self) -> bool {
        self.router.presence[self.unit].borrow().is_some()
    }

    fn pid(&self) -> Option<u32> {
        *self.router.presence[self.unit].borrow()
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<LayerCmd>) {
        // After a Core restart the old layer processes reconnect on their own: give them a
        // moment to be adopted before starting new ones.
        let mut presence = self.router.presence[self.unit].subscribe();
        let _ = tokio::time::timeout(Duration::from_secs(3), wait_presence(&mut presence, true)).await;
        if self.present() {
            tracing::info!("{}: adopted running process", self.unit);
            self.started = Some(Instant::now());
        }

        loop {
            if !self.present() {
                if let Err(e) = self.start().await {
                    tracing::error!("{}: start failed: {e}", self.unit);
                    self.on_crash();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
            match self.wait(&mut rx).await {
                Outcome::Down => {
                    tracing::warn!("{}: process went away unexpectedly", self.unit);
                    self.router.emit("host.notice", json!({ "text": format!("{} crashed; restarting", self.unit) }));
                    self.on_crash();
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Outcome::Cmd(LayerCmd::Swap { to, reply }) => {
                    let res = self.swap(&to).await;
                    let _ = reply.send(res);
                }
                Outcome::Cmd(LayerCmd::Stop { reply }) => {
                    self.shutdown().await;
                    let _ = reply.send(());
                    // Stay idle: accept no more work.
                    while let Some(cmd) = rx.recv().await {
                        match cmd {
                            LayerCmd::Swap { reply, .. } => {
                                let _ = reply.send(Err("stopped".into()));
                            }
                            LayerCmd::Stop { reply } => {
                                let _ = reply.send(());
                            }
                        }
                    }
                    return;
                }
            }
        }
    }

    async fn wait(&mut self, rx: &mut mpsc::UnboundedReceiver<LayerCmd>) -> Outcome {
        let mut presence = self.router.presence[self.unit].subscribe();
        loop {
            let promote_in = self
                .started
                .map(|s| PROMOTE_AFTER.saturating_sub(s.elapsed()))
                .unwrap_or(Duration::from_secs(3600));
            tokio::select! {
                _ = wait_presence(&mut presence, false) => return Outcome::Down,
                cmd = rx.recv() => match cmd {
                    Some(c) => return Outcome::Cmd(c),
                    None => std::future::pending::<()>().await,
                },
                _ = tokio::time::sleep(promote_in), if self.started.is_some() => {
                    self.promote();
                    self.started = None;
                }
            }
        }
    }

    /// Mark the running version as last-known-good once it has been up for a while.
    fn promote(&self) {
        let mut vs = VersionState::load();
        let u = vs.unit_mut(self.unit);
        if u.current.is_some() && u.last_good != u.current {
            u.last_good = u.current.clone();
            tracing::info!("{}: promoted {:?} to last known good", self.unit, u.current);
            let _ = vs.save();
        }
    }

    fn on_crash(&mut self) {
        let now = Instant::now();
        self.crashes.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        self.crashes.push(now);
        if self.crashes.len() >= 3 {
            let mut vs = VersionState::load();
            let u = vs.unit(self.unit);
            if u.last_good.is_some() && u.last_good != u.current {
                tracing::error!("{}: crash loop, rolling back to {:?}", self.unit, u.last_good);
                let lg = u.last_good.clone().unwrap();
                vs.set_current(self.unit, &lg);
                let _ = vs.save();
                self.router.emit(
                    "host.notice",
                    json!({ "text": format!("{} crash loop: rolled back to {lg}", self.unit) }),
                );
                crate::decisions::log(
                    &format!("Rolled back {} after a crash loop", self.unit),
                    &format!("3 crashes within 60s; reverted to last known good {lg}"),
                );
            }
            self.crashes.clear();
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        let vs = VersionState::load();
        let bin = tk_bus::versions::unit_bins(self.unit)[0];
        let exe = vs.exe(self.unit, bin, self.safe_mode).ok_or_else(|| format!("no version of {} installed", self.unit))?;
        tracing::info!("{}: starting {}", self.unit, exe.display());
        let mut cmd = tokio::process::Command::new(&exe);
        tk_bus::util::background_tokio(&mut cmd);
        cmd.env("TK_HOME", tk_bus::paths::home());
        if self.safe_mode {
            cmd.arg("--safe-mode");
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", exe.display()))?;

        let mut presence = self.router.presence[self.unit].subscribe();
        tokio::select! {
            r = tokio::time::timeout(READY_TIMEOUT, wait_presence(&mut presence, true)) => {
                if r.is_err() {
                    let _ = child.start_kill();
                    return Err("did not connect in time".into());
                }
            }
            status = child.wait() => {
                return Err(format!("exited during start: {status:?}"));
            }
        }
        // Reap the child in the background so it never becomes a zombie.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        self.started = Some(Instant::now());
        Ok(())
    }

    /// Wait for in-flight requests, ask the layer to hand off and exit, kill it if it won't.
    async fn shutdown(&mut self) {
        let unit = self.unit;
        self.router.st.lock().unwrap().draining.insert(unit.to_string());
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.router.inflight(unit) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let pid = self.pid();
        if self.present() {
            // The router queues calls to draining roles, so talk to the connection directly.
            let res = self.direct_call("layer.shutdown").await;
            if let Err(e) = res {
                tracing::warn!("{unit}: layer.shutdown failed: {e}");
            }
            let mut presence = self.router.presence[unit].subscribe();
            if tokio::time::timeout(Duration::from_secs(10), wait_presence(&mut presence, false)).await.is_err() {
                if let Some(pid) = pid {
                    tracing::warn!("{unit}: did not exit, killing pid {pid}");
                    tk_bus::util::kill_pid(pid);
                }
                let _ = tokio::time::timeout(Duration::from_secs(5), wait_presence(&mut presence, false)).await;
            }
        }
        self.started = None;
    }

    async fn direct_call(&self, method: &str) -> Result<serde_json::Value, String> {
        let unit = self.unit;
        self.router.st.lock().unwrap().draining.remove(unit);
        let res = self.router.call(unit, method, json!({}), Duration::from_secs(10)).await;
        self.router.st.lock().unwrap().draining.insert(unit.to_string());
        res
    }

    async fn swap(&mut self, to: &str) -> Result<(), String> {
        let unit = self.unit;
        let old = VersionState::load().unit(unit).current;
        tracing::info!("{unit}: swapping {old:?} -> {to}");
        self.shutdown().await;

        let mut vs = VersionState::load();
        vs.set_current(unit, to);
        vs.save().map_err(|e| e.to_string())?;
        self.router.st.lock().unwrap().draining.remove(unit);

        let result = match self.start().await {
            Ok(()) => self.check_stable().await,
            Err(e) => Err(e),
        };
        if let Err(e) = &result {
            tracing::error!("{unit}: new version {to} failed: {e}; rolling back to {old:?}");
            if self.present() {
                self.shutdown().await;
                self.router.st.lock().unwrap().draining.remove(unit);
            }
            if let Some(old) = &old {
                let mut vs = VersionState::load();
                vs.set_current(unit, old);
                let _ = vs.save();
            }
            if let Err(e2) = self.start().await {
                tracing::error!("{unit}: restarting old version failed too: {e2}");
            }
        }
        self.router.flush(unit);
        result
    }

    async fn check_stable(&self) -> Result<(), String> {
        let mut presence = self.router.presence[self.unit].subscribe();
        match tokio::time::timeout(STABLE_FOR, wait_presence(&mut presence, false)).await {
            Ok(_) => Err("crashed right after start".into()),
            Err(_) => Ok(()),
        }
    }
}

/// Wait until the role is up (`true`) or down (`false`). Returns a plain bool so no watch
/// guard is held across an await.
async fn wait_presence(rx: &mut tokio::sync::watch::Receiver<Option<u32>>, up: bool) -> bool {
    rx.wait_for(|p| p.is_some() == up).await.is_ok()
}

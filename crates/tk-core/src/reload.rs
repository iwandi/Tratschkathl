//! `host_reload`: build, stage, self-test, compatibility check, swap bottom-up, roll back on
//! failure. Jobs are persisted so a Core reload can finish the job in the new Core and answer
//! the (re-sent) call there.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use tk_bus::paths;
use tk_bus::util::write_atomic;
use tk_bus::versions::{self, VersionState};

use crate::router::Router;
use crate::supervisor::LayerHandle;

/// Units a reload can touch, in swap order (bottom-up). Helpers have no process to swap.
const RELOADABLE: &[&str] = &["helpers", "core", "services", "agents", "ui"];
const BUILD_PACKAGES: &[&str] = &["tk-core", "tk-services", "tk-agents", "tk-ui", "tk-shim", "tk-mcp-bridge", "tk-ctl"];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Job {
    pub id: String,
    pub status: String,
    pub reason: String,
    pub created_at: i64,
    pub plan: Vec<String>,
    pub staged: BTreeMap<String, String>,
    pub from: BTreeMap<String, Option<String>>,
    pub done: Vec<String>,
    pub failed: Option<(String, String)>,
    pub self_test: BTreeMap<String, Value>,
    pub log: Vec<String>,
    pub result: Option<Value>,
}

fn job_path(id: &str) -> PathBuf {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    paths::jobs_dir().join(format!("{safe}.json"))
}

fn load_job(id: &str) -> Option<Job> {
    std::fs::read(job_path(id)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save_job(job: &Job) {
    if let Ok(b) = serde_json::to_vec_pretty(job) {
        let _ = write_atomic(&job_path(&job.id), &b);
    }
}

pub struct Reloader {
    pub router: Arc<Router>,
    pub layers: HashMap<&'static str, LayerHandle>,
    pub exit_tx: mpsc::UnboundedSender<i32>,
    lock: tokio::sync::Mutex<()>,
}

impl Reloader {
    pub fn new(router: Arc<Router>, layers: HashMap<&'static str, LayerHandle>, exit_tx: mpsc::UnboundedSender<i32>) -> Self {
        Reloader { router, layers, exit_tx, lock: tokio::sync::Mutex::new(()) }
    }

    fn notice(&self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.router.emit("host.notice", json!({ "text": text }));
    }

    pub async fn host_reload(&self, idem: &str, args: &Value) -> Result<Value, String> {
        if let Some(job) = load_job(idem) {
            return self.wait_job(job).await;
        }
        let _guard = self.lock.lock().await;
        if let Some(job) = load_job(idem) {
            return self.wait_job(job).await;
        }
        let reason = args.get("reason").and_then(Value::as_str).unwrap_or("").to_string();
        let requested: Vec<String> = match args.get("layers") {
            Some(Value::Array(a)) if !a.is_empty() => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => RELOADABLE.iter().map(|s| s.to_string()).collect(),
        };
        for r in &requested {
            if !RELOADABLE.contains(&r.as_str()) {
                return Err(format!("unknown layer '{r}'; use one of {RELOADABLE:?}"));
            }
        }

        let vs = VersionState::load();
        let source = args
            .get("source_dir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or(vs.source_dir.clone())
            .ok_or("no source_dir known; start the launcher from a build directory or pass source_dir")?;

        self.notice(format!("reload: building {}", source.display()));
        let target = match build(&source).await {
            Ok(t) => t,
            Err(e) => {
                self.notice("reload: build failed");
                return Ok(json!({ "ok": false, "stage": "build", "errors": e }));
            }
        };

        // Which units changed?
        let mut job = Job {
            id: idem.to_string(),
            status: "running".into(),
            reason,
            created_at: tk_bus::model::now_ms(),
            ..Default::default()
        };
        for unit in RELOADABLE {
            if !requested.iter().any(|r| r == unit) {
                continue;
            }
            let id = versions::hash_unit(unit, &target).map_err(|e| e.to_string())?;
            let cur = vs.unit(unit).current;
            if cur.as_deref() == Some(id.as_str()) {
                continue;
            }
            let staged = versions::stage_unit(unit, &target).map_err(|e| format!("staging {unit}: {e}"))?;
            job.plan.push(unit.to_string());
            job.staged.insert(unit.to_string(), staged);
            job.from.insert(unit.to_string(), cur);
        }
        if job.plan.is_empty() {
            return Ok(json!({ "ok": true, "changed": {}, "note": "nothing changed" }));
        }

        // Self-test every staged unit.
        for unit in job.plan.clone() {
            let staged = &job.staged[&unit];
            for bin in versions::unit_bins(&unit) {
                if *bin == "tk-ctl" {
                    continue;
                }
                let exe = versions::version_dir(&unit, staged).join(paths::exe_name(bin));
                match self_test(&exe).await {
                    Ok(v) => {
                        job.self_test.insert(bin.to_string(), v);
                    }
                    Err(e) => {
                        self.notice(format!("reload: self-test of {bin} failed"));
                        return Ok(json!({ "ok": false, "stage": "self-test", "unit": unit, "errors": e }));
                    }
                }
            }
        }

        // Compatibility: every running unit outside the plan must speak the new protocol major.
        let new_major = job.self_test.values().filter_map(|v| v["proto"][0].as_u64()).max().unwrap_or(tk_bus::PROTO.0 as u64);
        let mut must_join = vec![];
        if !job.plan.iter().any(|u| u == "core") && tk_bus::PROTO.0 as u64 != new_major {
            must_join.push("core".to_string());
        }
        for layer in versions::LAYERS {
            if job.plan.iter().any(|u| u == layer) {
                continue;
            }
            if let Some(p) = self.router.proto_of(layer) {
                if p.0 as u64 != new_major {
                    must_join.push(layer.to_string());
                }
            }
        }
        if !must_join.is_empty() {
            return Ok(json!({ "ok": false, "stage": "compatibility",
                "errors": format!("protocol major {new_major} breaks running units {must_join:?}; reload them too") }));
        }

        job.log.push(format!("plan: {:?}", job.plan));
        save_job(&job);
        crate::decisions::log(
            &format!("Reload {:?}", job.plan),
            &format!("requested ({}); staged {:?}", job.reason, job.staged),
        );
        drop(_guard);
        self.execute(job).await
    }

    async fn wait_job(&self, mut job: Job) -> Result<Value, String> {
        loop {
            if job.status == "done" {
                return Ok(job.result.clone().unwrap_or(Value::Null));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            job = load_job(&job.id).ok_or("job vanished")?;
        }
    }

    async fn execute(&self, mut job: Job) -> Result<Value, String> {
        let _guard = self.lock.lock().await;
        for unit in job.plan.clone() {
            if job.done.contains(&unit) || job.failed.is_some() {
                continue;
            }
            let to = job.staged[&unit].clone();
            match unit.as_str() {
                "helpers" => {
                    let mut vs = VersionState::load();
                    vs.set_current("helpers", &to);
                    vs.save().map_err(|e| e.to_string())?;
                    job.done.push(unit.clone());
                    job.log.push(format!("helpers -> {to} (used by new sessions)"));
                }
                "core" => {
                    let mut vs = VersionState::load();
                    vs.set_current("core", &to);
                    vs.save().map_err(|e| e.to_string())?;
                    job.log.push(format!("core -> {to}: rebooting via launcher"));
                    save_job(&job);
                    self.notice("reload: rebooting Core");
                    let _ = self.exit_tx.send(tk_bus::EXIT_REBOOT);
                    // The new Core resumes this job.
                    std::future::pending::<()>().await;
                }
                layer => {
                    self.notice(format!("reload: swapping {layer}"));
                    match self.layers.get(layer) {
                        Some(h) => match h.swap(&to).await {
                            Ok(()) => {
                                job.done.push(unit.clone());
                                job.log.push(format!("{layer} -> {to}"));
                            }
                            Err(e) => {
                                job.log.push(format!("{layer} -> {to} FAILED: {e}; rolled back"));
                                job.failed = Some((unit.clone(), e));
                            }
                        },
                        None => {
                            // Layer not supervised (e.g. headless): just switch the version.
                            let mut vs = VersionState::load();
                            vs.set_current(layer, &to);
                            let _ = vs.save();
                            job.done.push(unit.clone());
                        }
                    }
                }
            }
            save_job(&job);
        }
        Ok(self.finish(job))
    }

    fn finish(&self, mut job: Job) -> Value {
        let changed: BTreeMap<_, _> = job
            .done
            .iter()
            .map(|u| (u.clone(), json!({ "from": job.from.get(u).cloned().flatten(), "to": job.staged.get(u) })))
            .collect();
        let result = json!({
            "ok": job.failed.is_none(),
            "changed": changed,
            "failed": job.failed.as_ref().map(|(u, e)| json!({ "unit": u, "error": e, "rolled_back": true })),
            "self_test": job.self_test,
            "log": job.log,
        });
        job.status = "done".into();
        job.result = Some(result.clone());
        save_job(&job);
        self.notice(if job.failed.is_none() {
            format!("reload done: {:?}", job.done)
        } else {
            format!("reload failed at {:?}; rolled back", job.failed.as_ref().map(|f| &f.0))
        });
        self.router.emit("host.reload", result.clone());
        if let Some((u, e)) = &job.failed {
            crate::decisions::log(&format!("Rolled back {u} during reload"), e);
        }
        result
    }

    /// Called at Core start: continue jobs a previous Core left running.
    pub async fn resume(self: Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(paths::jobs_dir()) else { return };
        for e in entries.flatten() {
            let Some(mut job) = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Job>(&b).ok()) else {
                continue;
            };
            if job.status != "running" {
                continue;
            }
            if job.plan.iter().any(|u| u == "core") && !job.done.iter().any(|u| u == "core") {
                let cur = VersionState::load().unit("core").current;
                if cur.as_ref() == job.staged.get("core") && std::env::args().all(|a| a != "--safe-mode") {
                    job.done.push("core".into());
                    job.log.push("core reboot ok".into());
                } else {
                    job.failed = Some(("core".into(), "new Core did not come up; launcher rolled back".into()));
                }
                save_job(&job);
            }
            tracing::info!("resuming reload job {}", job.id);
            let me = self.clone();
            tokio::spawn(async move {
                // Give layers a moment to be adopted by the new Core first.
                tokio::time::sleep(Duration::from_secs(4)).await;
                let _ = me.execute(job).await;
            });
        }
    }

    pub async fn host_rollback(&self, args: &Value) -> Result<Value, String> {
        let unit = args.get("layer").and_then(Value::as_str).ok_or("layer is required")?;
        let vs = VersionState::load();
        let u = vs.unit(unit);
        let target = if u.last_good.is_some() && u.last_good != u.current { u.last_good } else { u.previous }
            .ok_or("no earlier version to roll back to")?;
        let job = Job {
            id: format!("rollback-{}", tk_bus::model::now_ms()),
            status: "running".into(),
            reason: "rollback".into(),
            created_at: tk_bus::model::now_ms(),
            plan: vec![unit.to_string()],
            staged: [(unit.to_string(), target.clone())].into(),
            from: [(unit.to_string(), u.current.clone())].into(),
            ..Default::default()
        };
        save_job(&job);
        crate::decisions::log(&format!("Rollback {unit}"), &format!("to {target}"));
        self.execute(job).await
    }
}

async fn build(source: &Path) -> Result<PathBuf, String> {
    let meta = piped("cargo", &["metadata", "--format-version", "1", "--no-deps"], source).await?;
    if !meta.0 {
        return Err(meta.2);
    }
    let meta: Value = serde_json::from_str(&meta.1).map_err(|e| e.to_string())?;
    let target_dir = PathBuf::from(meta["target_directory"].as_str().ok_or("no target_directory")?);

    let mut args = vec!["build"];
    for p in BUILD_PACKAGES {
        args.push("-p");
        args.push(p);
    }
    let (ok, _out, err) = piped("cargo", &args, source).await?;
    if !ok {
        let lines: Vec<&str> = err.lines().collect();
        let tail = lines[lines.len().saturating_sub(80)..].join("\n");
        return Err(tail);
    }
    Ok(target_dir.join("debug"))
}

async fn piped(cmd: &str, args: &[&str], dir: &Path) -> Result<(bool, String, String), String> {
    let mut c = tokio::process::Command::new(cmd);
    c.args(args)
        .current_dir(dir)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    c.creation_flags(tk_bus::util::CREATE_NO_WINDOW);
    let out = c.output().await.map_err(|e| format!("running {cmd}: {e}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

async fn self_test(exe: &Path) -> Result<Value, String> {
    let mut c = tokio::process::Command::new(exe);
    c.arg("--self-test")
        .env("TK_HOME", paths::home())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(tk_bus::util::CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(60), c.output())
        .await
        .map_err(|_| "self-test timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!("exit {:?}: {}{}", out.status.code(), stdout, String::from_utf8_lossy(&out.stderr)));
    }
    let last = stdout.lines().rev().find(|l| l.trim_start().starts_with('{')).ok_or("self-test printed no JSON")?;
    serde_json::from_str(last).map_err(|e| e.to_string())
}

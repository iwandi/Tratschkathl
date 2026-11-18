//! tk-launcher: the external bootstrapper. Stages freshly built binaries into versioned
//! folders, starts Core, reboots it on request (exit code 75), and rolls it back when a new
//! Core does not come up or crash-loops.
//!
//! Usage: `tk-launcher [--from <build dir>] [--source <checkout>] [--safe-mode] [--headless]`

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use tk_bus::paths;
use tk_bus::versions::{self, VersionState, UNITS};

struct Args {
    from: Option<PathBuf>,
    source: Option<PathBuf>,
    safe_mode: bool,
    headless: bool,
    no_stage: bool,
    foreground: bool,
}

fn parse_args() -> Args {
    let mut a = Args { from: None, source: None, safe_mode: false, headless: false, no_stage: false, foreground: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--from" => a.from = it.next().map(PathBuf::from),
            "--source" => a.source = it.next().map(PathBuf::from),
            "--safe-mode" => a.safe_mode = true,
            "--headless" => a.headless = true,
            "--no-stage" => a.no_stage = true,
            "--foreground" => a.foreground = true,
            "--self-test" => {
                println!("{}", tk_bus::self_test_json("launcher"));
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    a
}

fn main() -> Result<()> {
    let args = parse_args();
    paths::ensure_dirs()?;
    tk_bus::logging::init("launcher");

    if core_running() {
        println!("Tratschkathl is already running (home: {}).", paths::home().display());
        return Ok(());
    }

    let self_exe = std::env::current_exe()?;
    let in_versions = self_exe.starts_with(paths::versions_dir());

    if !args.no_stage && !in_versions {
        let from = args.from.clone().unwrap_or_else(|| self_exe.parent().unwrap().to_path_buf());
        stage_all(&from, args.source.as_deref())?;
        // Re-exec from the versions folder so the build directory stays unlocked.
        let vs = VersionState::load();
        let exe = vs.exe("launcher", "tk-launcher", false).context("launcher not staged")?;
        let mut cmd = Command::new(exe);
        cmd.arg("--no-stage").env("TK_HOME", paths::home());
        if args.safe_mode {
            cmd.arg("--safe-mode");
        }
        if args.headless {
            cmd.arg("--headless");
        }
        if args.foreground {
            cmd.arg("--foreground");
            let status = cmd.status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        tk_bus::util::background_std(&mut cmd);
        cmd.spawn()?;
        println!("Tratschkathl started (home: {}). Logs: {}", paths::home().display(), paths::logs_dir().display());
        return Ok(());
    }

    run_core_loop(&args)
}

fn core_running() -> bool {
    use interprocess::local_socket::{prelude::*, GenericNamespaced, Stream};
    match paths::core_socket().to_ns_name::<GenericNamespaced>() {
        Ok(name) => Stream::connect(name).is_ok(),
        Err(_) => false,
    }
}

/// Copy every unit from a build directory into `versions/` and make it current.
fn stage_all(from: &Path, source: Option<&Path>) -> Result<()> {
    let mut vs = VersionState::load();
    for (unit, _) in UNITS {
        let id = versions::stage_unit(unit, from).with_context(|| format!("staging {unit} from {}", from.display()))?;
        if vs.unit(unit).current.as_deref() != Some(id.as_str()) {
            tracing::info!("staged {unit} {id}");
        }
        vs.set_current(unit, &id);
    }
    let source = source.map(Path::to_path_buf).or_else(|| versions::find_workspace_root(from));
    if source.is_some() {
        vs.source_dir = source;
    }
    vs.save()?;
    Ok(())
}

fn run_core_loop(args: &Args) -> Result<()> {
    let ready_file = paths::state_dir().join("core.ready");
    let mut crashes: Vec<Instant> = vec![];
    loop {
        let vs = VersionState::load();
        let exe = vs.exe("core", "tk-core", args.safe_mode).context("no Core version installed")?;
        let _ = std::fs::remove_file(&ready_file);
        tracing::info!("starting core {}", exe.display());
        let mut cmd = Command::new(&exe);
        tk_bus::util::background_std(&mut cmd);
        cmd.env("TK_HOME", paths::home());
        if args.safe_mode {
            cmd.arg("--safe-mode");
        }
        if args.headless {
            cmd.arg("--headless");
        }
        let mut child = cmd.spawn().with_context(|| format!("spawning {}", exe.display()))?;
        let started = Instant::now();

        // Wait for the ready file (or an early exit).
        let mut ready = false;
        while started.elapsed() < Duration::from_secs(30) {
            if ready_file.exists() {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait()? {
                tracing::error!("core exited during start: {status}");
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !ready {
            let _ = child.kill();
            let _ = child.wait();
            if !rollback_core("did not become ready") {
                anyhow::bail!("Core failed to start and there is nothing to roll back to");
            }
            continue;
        }

        // Promote to last-known-good after a minute of uptime.
        let mut promoted = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if !promoted && started.elapsed() > Duration::from_secs(60) {
                promoted = true;
                let mut vs = VersionState::load();
                let u = vs.unit_mut("core");
                if !args.safe_mode && u.last_good != u.current {
                    u.last_good = u.current.clone();
                    let _ = vs.save();
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        };

        match status.code() {
            Some(0) => {
                tracing::info!("core exited normally; launcher done");
                return Ok(());
            }
            Some(tk_bus::EXIT_REBOOT) => {
                tracing::info!("core asked for a reboot");
                continue;
            }
            code => {
                tracing::error!("core crashed ({code:?})");
                let now = Instant::now();
                crashes.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
                crashes.push(now);
                if crashes.len() >= 3 || started.elapsed() < Duration::from_secs(10) {
                    rollback_core("crashed shortly after start");
                    crashes.clear();
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Switch Core back to its last-known-good version. Returns false if there is none.
fn rollback_core(why: &str) -> bool {
    let mut vs = VersionState::load();
    let u = vs.unit("core");
    match u.last_good {
        Some(lg) if Some(&lg) != u.current.as_ref() => {
            tracing::error!("core {why}; rolling back to {lg}");
            vs.set_current("core", &lg);
            let _ = vs.save();
            true
        }
        _ => {
            tracing::error!("core {why}; no other version available");
            false
        }
    }
}

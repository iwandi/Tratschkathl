//! Process helpers.

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Configure a child so it has no console window and outlives its parent.
pub fn background_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
}

pub fn background_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false);
}

/// Fully detached (no console at all). Used for shims, which must survive everything.
pub fn detached_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
}

/// Force-kill a single process (not its children: shims must survive).
pub fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/F", "/PID", &pid.to_string()]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = std::process::Command::new("kill");
        c.args(["-9", &pid.to_string()]);
        c
    };
    background_std(&mut cmd);
    let _ = cmd.status();
}

/// The PATH as it is saved now (machine + user), not as it was when this process started.
/// Long-running layers use it so tools installed while the app runs (e.g. `claude`) are found.
pub fn fresh_path() -> std::ffi::OsString {
    #[cfg(windows)]
    {
        let read = |key: &str| -> Option<String> {
            let mut cmd = std::process::Command::new("reg");
            cmd.args(["query", key, "/v", "Path"]);
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let out = cmd.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines().find_map(|l| {
                let l = l.trim_start();
                let rest = l.strip_prefix("Path").or_else(|| l.strip_prefix("PATH"))?.trim_start();
                let rest = rest.strip_prefix("REG_EXPAND_SZ").or_else(|| rest.strip_prefix("REG_SZ"))?;
                Some(expand_env(rest.trim()))
            })
        };
        let machine = read(r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
        let user = read(r"HKCU\Environment");
        match (machine, user) {
            (None, None) => std::env::var_os("PATH").unwrap_or_default(),
            (m, u) => {
                // Keep entries this process added at start (e.g. a dev shell), after the saved ones.
                let mut parts: Vec<String> = [m, u].into_iter().flatten().collect();
                if let Some(cur) = std::env::var_os("PATH") {
                    parts.push(cur.to_string_lossy().into_owned());
                }
                parts.join(";").into()
            }
        }
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("PATH").unwrap_or_default()
    }
}

/// Expand `%VAR%` references (registry REG_EXPAND_SZ values).
pub fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Write a file atomically (temp file + rename).
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_vars_and_keeps_unknown() {
        std::env::set_var("TK_TEST_DIR", r"C:\x");
        assert_eq!(expand_env(r"%TK_TEST_DIR%\bin;%NOPE_TK%;50%"), r"C:\x\bin;%NOPE_TK%;50%");
    }

    #[test]
    fn fresh_path_is_not_empty() {
        assert!(!fresh_path().is_empty());
    }
}

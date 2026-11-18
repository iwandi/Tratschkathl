use std::path::PathBuf;

/// Root of all runtime data. `TK_HOME` overrides the default (`%LOCALAPPDATA%\Tratschkathl`).
pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("TK_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Tratschkathl")
}

pub fn state_dir() -> PathBuf {
    home().join("state")
}
pub fn versions_dir() -> PathBuf {
    home().join("versions")
}
pub fn logs_dir() -> PathBuf {
    home().join("logs")
}
pub fn sessions_dir() -> PathBuf {
    home().join("sessions")
}
pub fn jobs_dir() -> PathBuf {
    state_dir().join("jobs")
}
pub fn handoff_dir() -> PathBuf {
    state_dir().join("handoff")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [state_dir(), versions_dir(), logs_dir(), sessions_dir(), jobs_dir(), handoff_dir()] {
        std::fs::create_dir_all(d)?;
    }
    Ok(())
}

/// Short stable tag derived from the home path, so two homes never share socket names.
pub fn instance_tag() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in home().to_string_lossy().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h as u32)
}

pub fn core_socket() -> String {
    format!("tk-{}-core", instance_tag())
}

pub fn shim_socket(session: &str) -> String {
    format!("tk-{}-shim-{}", instance_tag(), session)
}

pub fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

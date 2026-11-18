//! Minimal decision log (spec §7.1): Core records every decision it takes on its own.
//! Append-only JSON lines in `<home>/decisions.jsonl`; Services takes this over in M3.

use std::io::Write;

pub fn log(title: &str, rationale: &str) {
    let entry = serde_json::json!({
        "at": tk_bus::model::now_ms(),
        "source": "core",
        "title": title,
        "rationale": rationale,
    });
    let path = tk_bus::paths::home().join("decisions.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{entry}");
    }
}

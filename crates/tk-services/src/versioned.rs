//! Agent-editable config files with history (spec §4.3, §6.1: "validated, versioned").
//! Every successful write keeps the previous text in `<home>/history/<name>/<timestamp>.toml`,
//! so any agent edit can be rolled back.

use std::path::PathBuf;

use tk_bus::paths;

pub struct VersionedFile {
    pub name: &'static str,
    pub default_text: &'static str,
}

impl VersionedFile {
    pub fn path(&self) -> PathBuf {
        paths::home().join(format!("{}.toml", self.name))
    }

    fn history_dir(&self) -> PathBuf {
        paths::home().join("history").join(self.name)
    }

    /// Current text, writing the default on first use.
    pub fn text(&self) -> String {
        match std::fs::read_to_string(self.path()) {
            Ok(t) => t,
            Err(_) => {
                let _ = std::fs::write(self.path(), self.default_text);
                self.default_text.to_string()
            }
        }
    }

    /// Earlier versions, newest first (ids are millisecond timestamps).
    pub fn versions(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.history_dir())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v.reverse();
        v
    }

    pub fn version_text(&self, id: &str) -> Result<String, String> {
        if !id.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("bad version id {id}"));
        }
        std::fs::read_to_string(self.history_dir().join(format!("{id}.toml"))).map_err(|_| format!("no version {id}"))
    }

    /// Validate with `check`, keep the old text in the history, write the new one.
    pub fn set(&self, text: &str, check: impl Fn(&str) -> Result<(), String>) -> Result<Option<String>, String> {
        check(text)?;
        let old = self.text();
        if old == text {
            return Ok(None);
        }
        let id = tk_bus::model::now_ms().to_string();
        std::fs::create_dir_all(self.history_dir()).map_err(|e| e.to_string())?;
        std::fs::write(self.history_dir().join(format!("{id}.toml")), old).map_err(|e| e.to_string())?;
        tk_bus::util::write_atomic(&self.path(), text.as_bytes()).map_err(|e| e.to_string())?;
        Ok(Some(id))
    }
}

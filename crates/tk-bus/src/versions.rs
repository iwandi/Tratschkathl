//! Versioned unit binaries: `versions/<unit>/<id>/<exe>` plus `state/versions.json` with the
//! current / previous / last-known-good id per unit. Running executables are never overwritten
//! (Windows locks them), a new build always lands in a new folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::util::write_atomic;

/// Unit name → binaries it consists of.
pub const UNITS: &[(&str, &[&str])] = &[
    ("launcher", &["tk-launcher"]),
    ("core", &["tk-core"]),
    ("services", &["tk-services"]),
    ("agents", &["tk-agents"]),
    ("ui", &["tk-ui"]),
    ("helpers", &["tk-shim", "tk-mcp-bridge", "tk-ctl"]),
];

/// Units that run as reloadable layer processes supervised by Core, in swap order.
pub const LAYERS: &[&str] = &["services", "agents", "ui"];

pub fn unit_bins(unit: &str) -> &'static [&'static str] {
    UNITS.iter().find(|(u, _)| *u == unit).map(|(_, b)| *b).unwrap_or(&[])
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct UnitVersions {
    pub current: Option<String>,
    pub previous: Option<String>,
    pub last_good: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct VersionState {
    #[serde(default)]
    pub units: BTreeMap<String, UnitVersions>,
    /// Source checkout used by `host_reload` builds.
    #[serde(default)]
    pub source_dir: Option<PathBuf>,
}

impl VersionState {
    pub fn path() -> PathBuf {
        paths::state_dir().join("versions.json")
    }

    pub fn load() -> VersionState {
        std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&Self::path(), &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn unit(&self, unit: &str) -> UnitVersions {
        self.units.get(unit).cloned().unwrap_or_default()
    }

    pub fn unit_mut(&mut self, unit: &str) -> &mut UnitVersions {
        self.units.entry(unit.to_string()).or_default()
    }

    /// Make `id` current, remembering the old one as previous.
    pub fn set_current(&mut self, unit: &str, id: &str) {
        let u = self.unit_mut(unit);
        if u.current.as_deref() != Some(id) {
            u.previous = u.current.take();
            u.current = Some(id.to_string());
        }
        if u.last_good.is_none() {
            u.last_good = Some(id.to_string());
        }
    }

    /// Path of a binary in the unit's current (or last-good in safe mode) version.
    pub fn exe(&self, unit: &str, bin: &str, safe_mode: bool) -> Option<PathBuf> {
        let u = self.unit(unit);
        let id = if safe_mode { u.last_good.or(u.current)? } else { u.current? };
        Some(version_dir(unit, &id).join(paths::exe_name(bin)))
    }
}

pub fn version_dir(unit: &str, id: &str) -> PathBuf {
    paths::versions_dir().join(unit).join(id)
}

/// Content hash of a unit's binaries in `dir` (first 12 hex chars of SHA-256).
pub fn hash_unit(unit: &str, dir: &Path) -> Result<String> {
    let mut h = Sha256::new();
    for bin in unit_bins(unit) {
        let p = dir.join(paths::exe_name(bin));
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        h.update(bin.as_bytes());
        h.update(&bytes);
    }
    Ok(format!("{:x}", h.finalize())[..12].to_string())
}

/// Copy a unit's binaries from `dir` into its version folder. Returns the version id.
pub fn stage_unit(unit: &str, dir: &Path) -> Result<String> {
    let id = hash_unit(unit, dir)?;
    let dest = version_dir(unit, &id);
    if !dest.join(paths::exe_name(unit_bins(unit)[0])).exists() {
        let tmp = dest.with_extension("staging");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        for bin in unit_bins(unit) {
            let name = paths::exe_name(bin);
            std::fs::copy(dir.join(&name), tmp.join(&name))
                .with_context(|| format!("copying {name}"))?;
        }
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(id)
}

/// Find the Cargo workspace root at or above `start`.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let toml = d.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&toml) {
            if text.contains("[workspace]") {
                return Some(d.to_path_buf());
            }
        }
        dir = d.parent();
    }
    None
}

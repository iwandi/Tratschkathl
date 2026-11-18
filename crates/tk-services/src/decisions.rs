//! Decision log (spec §7.1) as files in the project, so decisions are reviewed and committed
//! with the code: `<project>/.tk/decisions/<YYYYMMDD-HHMMSS>-<slug>.md`, one file per decision
//! (parallel sessions and branches never conflict).
//!
//! Format: TOML front matter between `+++` lines, then Markdown sections.
//!
//! ```text
//! +++
//! title = "Decision log is a file in the project"
//! date = "2026-11-18T23:40:00Z"
//! status = "accepted"            # or "superseded"
//! source = "agent"               # agent | user
//! session = "s…"
//! supersedes = "20261118-232500-decision-log-lives-in-services.md"
//! [tags]
//! issue = "TK-1"
//! +++
//!
//! ## Context
//! …
//! ## Decision
//! …
//! ## Alternatives
//! …
//! ## Why
//! …
//! ```
//!
//! Core's automatic decisions (reloads, rollbacks) are about the host, not a project: they stay
//! in `<home>/decisions.jsonl` and are read from there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Facts that describe where a decision was taken but are machine- or project-implied.
const SKIP_TAGS: &[&str] = &["worktree", "repo", "project"];

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Decision {
    /// File name inside `.tk/decisions/`.
    #[serde(default)]
    pub file: String,
    pub title: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub alternatives: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct FrontMatter {
    title: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tags: BTreeMap<String, String>,
}

pub fn dir(project: &Path) -> PathBuf {
    project.join(".tk").join("decisions")
}

/// Tags for a decision from the session's facts (first value per key).
pub fn tags_from(facts: &[tk_bus::model::Fact]) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    for f in facts {
        if !SKIP_TAGS.contains(&f.key.as_str()) {
            tags.entry(f.key.clone()).or_insert_with(|| f.value.clone());
        }
    }
    tags
}

fn slug(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
    let mut out = s.join("-");
    out.truncate(48);
    out.trim_end_matches('-').to_string()
}

/// `2026-11-18T23:40:05Z` and `20261118-234005` for epoch milliseconds (UTC).
fn stamps(ms: i64) -> (String, String) {
    let secs = ms / 1000;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    (format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z"), format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}"))
}

fn render(d: &Decision) -> String {
    let fm = FrontMatter {
        title: d.title.clone(),
        date: d.date.clone(),
        status: d.status.clone(),
        source: d.source.clone(),
        session: d.session.clone(),
        supersedes: d.supersedes.clone(),
        superseded_by: d.superseded_by.clone(),
        tags: d.tags.clone(),
    };
    let mut out = format!("+++\n{}+++\n", toml::to_string(&fm).unwrap_or_default());
    for (heading, body) in [
        ("Context", &d.context),
        ("Decision", &d.decision),
        ("Alternatives", &d.alternatives),
        ("Why", &d.rationale),
    ] {
        if let Some(b) = body.as_deref().filter(|b| !b.trim().is_empty()) {
            out.push_str(&format!("\n## {heading}\n\n{}\n", b.trim()));
        }
    }
    out
}

fn parse(file: &str, text: &str) -> Option<Decision> {
    let rest = text.strip_prefix("+++")?.trim_start_matches(['\r', '\n']);
    let end = rest.find("\n+++")?;
    let fm: FrontMatter = toml::from_str(&rest[..end]).ok()?;
    let body = &rest[end + 4..];
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            current = Some(h.trim().to_string());
            continue;
        }
        if let Some(c) = &current {
            let e = sections.entry(c.clone()).or_default();
            e.push_str(line);
            e.push('\n');
        }
    }
    let take = |k: &str| sections.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Some(Decision {
        file: file.to_string(),
        title: fm.title,
        date: fm.date,
        status: fm.status,
        source: fm.source,
        session: fm.session,
        supersedes: fm.supersedes,
        superseded_by: fm.superseded_by,
        tags: fm.tags,
        context: take("Context"),
        decision: take("Decision"),
        alternatives: take("Alternatives"),
        rationale: take("Why"),
    })
}

/// Write a new decision file. If it supersedes another, that one is marked superseded.
pub fn log(project: &Path, mut d: Decision, now_ms: i64) -> Result<Decision, String> {
    if d.title.trim().is_empty() {
        return Err("a decision needs a title".into());
    }
    let dir = dir(project);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (date, stamp) = stamps(now_ms);
    d.date = date;
    if d.status.is_empty() {
        d.status = "accepted".into();
    }
    let mut file = format!("{stamp}-{}.md", slug(&d.title));
    let mut n = 2;
    while dir.join(&file).exists() {
        file = format!("{stamp}-{}-{n}.md", slug(&d.title));
        n += 1;
    }
    d.file = file.clone();
    if let Some(old) = d.supersedes.clone() {
        let path = dir.join(&old);
        let text = std::fs::read_to_string(&path).map_err(|_| format!("supersedes: no decision file {old}"))?;
        let mut prev = parse(&old, &text).ok_or_else(|| format!("supersedes: cannot parse {old}"))?;
        prev.status = "superseded".into();
        prev.superseded_by = Some(file.clone());
        std::fs::write(&path, render(&prev)).map_err(|e| e.to_string())?;
    }
    std::fs::write(dir.join(&file), render(&d)).map_err(|e| e.to_string())?;
    Ok(d)
}

/// The project's decisions matching every tag in `filter`, newest first.
pub fn list(project: &Path, filter: &BTreeMap<String, String>, limit: usize) -> Vec<Decision> {
    let Ok(rd) = std::fs::read_dir(dir(project)) else { return vec![] };
    let mut all: Vec<Decision> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            std::fs::read_to_string(e.path()).ok().and_then(|t| parse(&name, &t))
        })
        .filter(|d| filter.iter().all(|(k, v)| d.tags.get(k) == Some(v)))
        .collect();
    all.sort_by(|a, b| b.file.cmp(&a.file));
    all.truncate(limit);
    all
}

/// Core's automatic decisions about the host (`<home>/decisions.jsonl`), newest first.
pub fn host(limit: usize) -> Vec<Decision> {
    let text = std::fs::read_to_string(tk_bus::paths::home().join("decisions.jsonl")).unwrap_or_default();
    let mut out: Vec<Decision> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| Decision {
            title: v["title"].as_str().unwrap_or("").to_string(),
            date: stamps(v["at"].as_i64().unwrap_or(0)).0,
            status: "accepted".into(),
            source: v["source"].as_str().unwrap_or("core").to_string(),
            rationale: v["rationale"].as_str().map(String::from),
            ..Default::default()
        })
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_parse_supersede_and_filter() {
        let project = std::env::temp_dir().join(format!("tk-dec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        let mut tags = BTreeMap::new();
        tags.insert("issue".to_string(), "TK-1".to_string());
        let first = log(
            &project,
            Decision { title: "Use SQLite".into(), source: "agent".into(), decision: Some("SQLite".into()), tags: tags.clone(), ..Default::default() },
            1_795_000_000_000,
        )
        .unwrap();
        let second = log(
            &project,
            Decision {
                title: "Use files".into(),
                source: "user".into(),
                rationale: Some("Reviewable and committed.".into()),
                supersedes: Some(first.file.clone()),
                ..Default::default()
            },
            1_795_000_060_000,
        )
        .unwrap();
        let all = list(&project, &BTreeMap::new(), 10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].file, second.file);
        assert_eq!(all[0].rationale.as_deref(), Some("Reviewable and committed."));
        assert_eq!(all[1].status, "superseded");
        assert_eq!(all[1].superseded_by.as_deref(), Some(second.file.as_str()));
        assert_eq!(all[1].decision.as_deref(), Some("SQLite"));
        assert_eq!(list(&project, &tags, 10).len(), 1);
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn stamps_and_slugs() {
        assert_eq!(stamps(0).0, "1970-01-01T00:00:00Z");
        assert_eq!(slug("Decision log: a file, not SQLite!"), "decision-log-a-file-not-sqlite");
    }
}

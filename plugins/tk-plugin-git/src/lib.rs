//! Git plugin. M1: `FactDeriver` — derives `project`, `repo`, `branch` and `worktree` from a
//! session's working folder. Later: `Vcs` (worktrees, changes, diff) and `DocumentProvider`
//! (worktree changes as a `changeset` document).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tk_plugin_api::{
    doc_uri, reference, text, Capability, DocRef, Document, DocumentProvider, Fact, FactDeriver, FactInput, Plugin,
    SessionContext,
};

pub struct GitPlugin;

impl Plugin for GitPlugin {
    fn id(&self) -> &'static str {
        "git"
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::FactDeriver, Capability::Vcs, Capability::DocumentProvider]
    }
    fn fact_deriver(&self) -> Option<&dyn FactDeriver> {
        Some(self)
    }
    fn document_provider(&self) -> Option<&dyn DocumentProvider> {
        Some(self)
    }
}

/// Cap on diff text per changeset, so huge diffs never flood the bus.
const MAX_DIFF_BYTES: usize = 400 * 1024;
const MAX_UNTRACKED_LINES: usize = 200;

impl DocumentProvider for GitPlugin {
    /// One `changeset` per worktree fact: the worktree's uncommitted changes.
    fn list(&self, facts: &[Fact]) -> Vec<DocRef> {
        let branch = facts.iter().find(|f| f.key == "branch").map(|f| f.value.clone());
        facts
            .iter()
            .filter(|f| f.key == "worktree")
            .map(|f| DocRef {
                uri: doc_uri("changeset", "git", &f.value),
                kind: "changeset".into(),
                title: match &branch {
                    Some(b) => format!("Changes · {b}"),
                    None => "Changes".into(),
                },
                provider: "git".into(),
            })
            .collect()
    }

    fn get(&self, kind: &str, reference: &str) -> Result<Document, String> {
        if kind != "changeset" {
            return Err(format!("git: unknown document kind {kind}"));
        }
        let root = PathBuf::from(reference);
        git(&root, &["rev-parse", "--show-toplevel"]).ok_or_else(|| format!("{reference} is not a git worktree"))?;
        let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
        Ok(Document {
            uri: doc_uri("changeset", "git", reference),
            kind: "changeset".into(),
            title: if branch.is_empty() { "Changes".into() } else { format!("Changes · {branch}") },
            body: changeset(&root, &branch),
        })
    }
}

fn changeset(root: &Path, branch: &str) -> serde_json::Value {
    use serde_json::json;
    // Tracked changes against HEAD (staged and unstaged together).
    let numstat = git_raw(root, &["diff", "HEAD", "--numstat", "--no-renames"]).unwrap_or_default();
    let mut counts = std::collections::HashMap::new();
    for line in numstat.lines() {
        let mut it = line.splitn(3, '\t');
        if let (Some(a), Some(r), Some(p)) = (it.next(), it.next(), it.next()) {
            counts.insert(p.to_string(), (a.parse::<u64>().unwrap_or(0), r.parse::<u64>().unwrap_or(0)));
        }
    }
    let mut diff = git_raw(root, &["diff", "HEAD", "--no-color", "--no-ext-diff", "--no-renames"]).unwrap_or_default();
    let mut truncated = false;
    if diff.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        truncated = true;
    }
    let mut files = vec![];
    // A file starts at a line beginning with "diff --git " (never split inside content:
    // the text can appear in a changed file, e.g. this one).
    let mut chunks: Vec<Vec<&str>> = vec![];
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            chunks.push(vec![line]);
        } else if let Some(c) = chunks.last_mut() {
            c.push(line);
        }
    }
    for chunk in chunks {
        let header = chunk[0].trim_start_matches("diff --git ");
        let path = header.rsplit(" b/").next().unwrap_or(header).to_string();
        let hunks_at = chunk.iter().position(|l| l.starts_with("@@")).unwrap_or(chunk.len());
        let meta = &chunk[..hunks_at];
        let status = if meta.iter().any(|l| l.starts_with("new file mode")) {
            "A"
        } else if meta.iter().any(|l| l.starts_with("deleted file mode")) {
            "D"
        } else {
            "M"
        };
        let body = chunk[hunks_at..].join("\n");
        let (added, removed) = counts.get(&path).copied().unwrap_or((0, 0));
        files.push(json!({ "path": path, "status": status, "added": added, "removed": removed, "diff": body }));
    }
    // Untracked files show as additions.
    let untracked = git_raw(root, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    for path in untracked.lines().filter(|l| !l.is_empty()) {
        let content = std::fs::read(root.join(path)).ok();
        let (diff, added) = match content.as_deref().map(std::str::from_utf8) {
            Some(Ok(text)) => {
                let lines: Vec<&str> = text.lines().collect();
                let shown: Vec<String> = lines.iter().take(MAX_UNTRACKED_LINES).map(|l| format!("+{l}")).collect();
                let mut d = format!("@@ -0,0 +1,{} @@\n{}", lines.len(), shown.join("\n"));
                if lines.len() > MAX_UNTRACKED_LINES {
                    d.push_str(&format!("\n… {} more lines", lines.len() - MAX_UNTRACKED_LINES));
                }
                (d, lines.len() as u64)
            }
            _ => ("(binary or unreadable)".to_string(), 0),
        };
        files.push(json!({ "path": path, "status": "?", "added": added, "removed": 0, "diff": diff }));
    }
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!({ "root": root.to_string_lossy(), "branch": branch, "files": files, "truncated": truncated })
}

impl FactDeriver for GitPlugin {
    fn derive(&self, ctx: &SessionContext) -> Vec<FactInput> {
        let cwd = &ctx.cwd;
        let Some(top) = git(cwd, &["rev-parse", "--show-toplevel"]) else {
            return vec![];
        };
        // The common dir is `<main checkout>/.git` for every worktree of the repository, so
        // its parent names the repository even inside a linked worktree.
        let main_root = git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(PathBuf::from)
            .and_then(|common| {
                if common.file_name().map(|n| n == ".git").unwrap_or(false) {
                    common.parent().map(Path::to_path_buf)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| PathBuf::from(&top));
        let repo = main_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| top.clone());

        let mut facts = vec![text("project", repo.clone()), text("repo", repo), text("worktree", top)];
        match git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) {
            Some(b) if b != "HEAD" => facts.push(reference("branch", "git.branch", b)),
            Some(_) => {
                if let Some(sha) = git(cwd, &["rev-parse", "--short", "HEAD"]) {
                    facts.push(reference("branch", "git.commit", format!("detached@{sha}")));
                }
            }
            None => {}
        }
        facts
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let s = git_raw(cwd, args)?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Run git and return stdout untrimmed (diffs keep their whitespace).
fn git_raw(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(tk_bus::util::CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_facts_for_this_checkout() {
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let facts = GitPlugin.derive(&SessionContext { session: "t".into(), cwd });
        let keys: Vec<_> = facts.iter().map(|f| f.key.as_str()).collect();
        assert!(keys.contains(&"project"), "{facts:?}");
        assert!(keys.contains(&"worktree"), "{facts:?}");
    }

    #[test]
    fn changeset_document_loads() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let top = git(&root, &["rev-parse", "--show-toplevel"]).unwrap();
        let doc = GitPlugin.get("changeset", &top).unwrap();
        assert_eq!(doc.kind, "changeset");
        // Every entry is a real path (no fragments from splitting inside file content).
        for f in doc.body["files"].as_array().unwrap() {
            let path = f["path"].as_str().unwrap();
            assert!(!path.contains(' ') || root.join("..").join("..").join(path).exists(), "bogus path {path}");
            assert!(!path.contains('('), "bogus path {path}");
        }
    }

    #[test]
    fn no_facts_outside_git() {
        let facts = GitPlugin.derive(&SessionContext { session: "t".into(), cwd: std::env::temp_dir() });
        assert!(facts.is_empty() || facts.iter().all(|f| f.key != "branch" || f.kind.is_some()));
    }
}

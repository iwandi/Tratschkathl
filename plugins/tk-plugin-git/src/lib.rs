//! Git plugin. M1: `FactDeriver` — derives `project`, `repo`, `branch` and `worktree` from a
//! session's working folder. Later: `Vcs` (worktrees, changes, diff) and `DocumentProvider`
//! (worktree changes as a `changeset` document).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tk_plugin_api::{reference, text, Capability, FactDeriver, FactInput, Plugin, SessionContext};

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
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
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
    fn no_facts_outside_git() {
        let facts = GitPlugin.derive(&SessionContext { session: "t".into(), cwd: std::env::temp_dir() });
        assert!(facts.is_empty() || facts.iter().all(|f| f.key != "branch" || f.kind.is_some()));
    }
}

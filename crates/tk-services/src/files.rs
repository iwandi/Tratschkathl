//! Built-in "files" provider (spec §4.4: Markdown view, provided by core): offers the
//! Markdown files of a session's worktree (root and `docs/`) plus any `doc` facts.

use std::path::{Path, PathBuf};

use serde_json::json;

use tk_plugin_api::{doc_uri, Capability, DocRef, Document, DocumentProvider, Fact, Plugin};

const MAX_FILES: usize = 16;
const MAX_BYTES: u64 = 1024 * 1024;

pub struct FilesPlugin;

impl Plugin for FilesPlugin {
    fn id(&self) -> &'static str {
        "files"
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::DocumentProvider]
    }
    fn document_provider(&self) -> Option<&dyn DocumentProvider> {
        Some(self)
    }
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn markdown_in(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")))
        .collect();
    found.sort();
    out.extend(found);
}

impl DocumentProvider for FilesPlugin {
    fn list(&self, facts: &[Fact]) -> Vec<DocRef> {
        let mut paths: Vec<PathBuf> = vec![];
        let worktree = facts.iter().find(|f| f.key == "worktree").map(|f| PathBuf::from(&f.value));
        // Explicit `doc` facts first: what the agent says matters.
        for f in facts.iter().filter(|f| f.key == "doc") {
            let p = PathBuf::from(&f.value);
            let p = match (&worktree, p.is_relative()) {
                (Some(w), true) => w.join(p),
                _ => p,
            };
            if p.is_file() {
                paths.push(p);
            }
        }
        if let Some(w) = &worktree {
            markdown_in(w, &mut paths);
            markdown_in(&w.join("docs"), &mut paths);
        }
        let mut seen = std::collections::HashSet::new();
        paths
            .into_iter()
            .filter(|p| seen.insert(norm(p).to_lowercase()))
            .take(MAX_FILES)
            .map(|p| {
                let title = match &worktree {
                    Some(w) => p.strip_prefix(w).map(norm).unwrap_or_else(|_| norm(&p)),
                    None => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                };
                DocRef { uri: doc_uri("markdown", "files", &norm(&p)), kind: "markdown".into(), title, provider: "files".into() }
            })
            .collect()
    }

    fn get(&self, kind: &str, reference: &str) -> Result<Document, String> {
        if kind != "markdown" {
            return Err(format!("files: unknown document kind {kind}"));
        }
        let path = PathBuf::from(reference);
        let meta = std::fs::metadata(&path).map_err(|e| format!("{reference}: {e}"))?;
        if meta.len() > MAX_BYTES {
            return Err(format!("{reference} is larger than 1 MB"));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{reference}: {e}"))?;
        Ok(Document {
            uri: doc_uri("markdown", "files", reference),
            kind: "markdown".into(),
            title: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            body: json!({ "path": reference, "text": text }),
        })
    }
}

//! Structural AST rewrite with a preview-then-accept flow.
//!
//! `ast_edit` runs a dry ast-grep rewrite, stages the proposed new content in a session-scoped
//! `PendingEditStore`, and returns a replacement count + diff preview. Nothing is written.
//! `resolve` then commits (all-or-nothing per file) or discards the staged edit by `edit_id`.
//! A discarded proposal leaves the tree untouched. Staged edits are cleared on compaction so a
//! stale proposal can never write a pre-compaction snapshot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ast_grep_language::LanguageExt;
use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::astgrep::{count_changes, has_error_or_missing, parse_lang, unified_diff};
use super::{relative_path, walk_builder_opts};

const MAX_PENDING_EDITS: usize = 32;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct AstEdit {
    #[param(description = "AST pattern with $VAR and $$$BODY metavariables")]
    pattern: String,
    #[param(description = "Replacement pattern, using $VAR refs from the pattern")]
    rewrite: String,
    #[param(description = "Language: rust, typescript, tsx, python, go")]
    lang: String,
    #[param(description = "Directory or file to search (default: cwd)")]
    path: Option<String>,
    #[param(description = "Glob patterns to include (e.g. [\"*.rs\", \"src/**\"])")]
    globs: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PendingFile {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone)]
pub struct PendingEdit {
    pub edit_id: String,
    pub pattern: String,
    pub lang: String,
    pub files: Vec<PendingFile>,
    pub replacements: usize,
}

#[derive(Debug, Default)]
pub struct PendingEditStore(Mutex<HashMap<String, PendingEdit>>);

impl PendingEditStore {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn stage(&self, edit: PendingEdit) -> Result<(), String> {
        let mut guard = self.0.lock().unwrap();
        if guard.len() >= MAX_PENDING_EDITS && !guard.contains_key(&edit.edit_id) {
            return Err(format!(
                "too many pending edits (max {MAX_PENDING_EDITS}); resolve or discard one first"
            ));
        }
        guard.insert(edit.edit_id.clone(), edit);
        Ok(())
    }

    pub fn take(&self, edit_id: &str) -> Option<PendingEdit> {
        self.0.lock().unwrap().remove(edit_id)
    }

    pub fn discard(&self, edit_id: &str) -> bool {
        self.0.lock().unwrap().remove(edit_id).is_some()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    pub fn list(&self) -> Vec<PendingEdit> {
        self.0.lock().unwrap().values().cloned().collect()
    }
}

impl AstEdit {
    pub const NAME: &str = "ast_edit";
    pub const DESCRIPTION: &str = include_str!("ast_edit.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"pattern": "console.log($MSG)", "rewrite": "tracing::info!($MSG)", "lang": "typescript"},
  {"pattern": "Vec::new()", "rewrite": "vec![]", "lang": "rust", "path": "src/"}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let lang = parse_lang(&self.lang)?;
        let search_path = self.path.clone().unwrap_or_else(|| ".".into());
        let globs = self.globs.clone().unwrap_or_default();
        let lang_types = lang.file_types();

        let walk_path = search_path.clone();
        let paths = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>, String> {
            let glob_refs: Vec<&str> = globs.iter().map(|s| s.as_str()).collect();
            let mut builder =
                walk_builder_opts(&walk_path, &glob_refs, true).map_err(|e| e.to_string())?;
            builder.types(lang_types);
            let mut out = Vec::new();
            for entry in builder.build().flatten() {
                if entry.file_type().is_some_and(|ft| ft.is_file()) {
                    out.push(entry.into_path());
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| format!("ast_edit walk failed: {e}"))??;

        let mut pending_files = Vec::new();
        let mut files_matched = 0u64;
        let mut files_scanned = 0u64;
        let mut total_replacements = 0usize;
        let mut diffs = Vec::new();

        for path in paths {
            let content = match ctx.fs.read_text_file(&path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            ctx.file_tracker.record_read(&path);
            files_scanned += 1;

            let mut grep = lang.ast_grep(&content);
            let edits = grep
                .root()
                .replace_all(self.pattern.as_str(), self.rewrite.as_str());
            if edits.is_empty() {
                continue;
            }
            for edit in edits.into_iter().rev() {
                grep.edit(edit).map_err(|e| e.to_string())?;
            }
            let new_content = grep.generate();
            if new_content == content {
                continue;
            }

            let repl_grep = lang.ast_grep(&new_content);
            if has_error_or_missing(&repl_grep.root()) {
                return Err(format!(
                    "{}: rewrite introduces syntax errors; refusing to stage",
                    relative_path(&path.to_string_lossy())
                ));
            }

            files_matched += 1;
            let diff_count = count_changes(&content, &new_content);
            total_replacements = total_replacements.saturating_add(diff_count);
            let rel = relative_path(&path.to_string_lossy());
            diffs.push(unified_diff(&content, &new_content, &rel));
            pending_files.push(PendingFile {
                path,
                before: content,
                after: new_content,
            });
        }

        if pending_files.is_empty() {
            return Ok(ToolOutput::Plain(format!(
                "no matches for \"{}\" in {search_path} ({files_scanned} files scanned)",
                self.pattern
            )));
        }

        let edit_id = make_edit_id();
        let pending = PendingEdit {
            edit_id: edit_id.clone(),
            pattern: self.pattern.clone(),
            lang: self.lang.clone(),
            replacements: total_replacements,
            files: pending_files,
        };
        ctx.pending_edits.stage(pending)?;

        let header = format!(
            "(proposed) edit_id={edit_id}: \"{pat}\" [{lang}] in {search_path}\n{matched}/{scanned} files, {repl} replacement(s)\n",
            pat = self.pattern,
            lang = self.lang,
            matched = files_matched,
            scanned = files_scanned,
            repl = total_replacements,
        );
        let body = truncate_diffs(&diffs, 30_000);
        Ok(ToolOutput::Plain(header + &body))
    }

    pub fn start_header(&self) -> String {
        format!("ast_edit {} -> {}", self.pattern, self.rewrite)
    }
}

fn make_edit_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("e{ts:x}{seq:x}")
}

fn truncate_diffs(diffs: &[String], max_bytes: usize) -> String {
    let mut out = String::new();
    for d in diffs {
        if out.len() + d.len() + 1 > max_bytes {
            out.push_str("\n... (diff truncated)");
            break;
        }
        out.push_str(d);
        out.push('\n');
    }
    out
}

super::impl_tool!(
    AstEdit,
    audience = super::ToolAudience::MAIN,
    kind = "ast_edit",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for AstEdit {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(AstEdit::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { AstEdit::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn pending_store_stage_take_discard() {
        let store = PendingEditStore::fresh();
        let edit = PendingEdit {
            edit_id: "e1".into(),
            pattern: "a".into(),
            lang: "rust".into(),
            replacements: 1,
            files: vec![],
        };
        store.stage(edit).unwrap();
        assert!(store.take("e1").is_some());
        assert!(store.take("e1").is_none());
    }

    #[test]
    fn pending_store_discard_returns_bool() {
        let store = PendingEditStore::fresh();
        assert!(!store.discard("missing"));
    }

    #[tokio::test]
    async fn dry_run_stages_edit_no_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        fs::write(&path, "Vec::new();\n").unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let edit = AstEdit::parse_input(&json!({
            "pattern": "Vec::new()",
            "rewrite": "vec![]",
            "lang": "rust",
            "path": path.to_str().unwrap(),
        }))
        .unwrap();
        let out = edit.execute(&ctx).await.unwrap();
        let text = out.as_text().to_string();
        assert!(text.contains("(proposed)"), "{text}");
        assert!(text.contains("1 replacement"), "{text}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "Vec::new();\n");
        let pending = ctx.pending_edits.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].replacements, 1);
    }

    #[tokio::test]
    async fn no_match_returns_plain() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let edit = AstEdit::parse_input(&json!({
            "pattern": "Vec::new()",
            "rewrite": "vec![]",
            "lang": "rust",
            "path": path.to_str().unwrap(),
        }))
        .unwrap();
        let out = edit.execute(&ctx).await.unwrap();
        assert!(out.as_text().contains("no matches"));
        assert!(ctx.pending_edits.list().is_empty());
    }

    #[tokio::test]
    async fn syntax_error_rewrite_refuses_to_stage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        fs::write(&path, "fn valid() {}\n").unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let edit = AstEdit::parse_input(&json!({
            "pattern": "fn valid() {}",
            "rewrite": "fn { broken",
            "lang": "rust",
            "path": path.to_str().unwrap(),
        }))
        .unwrap();
        let err = edit.execute(&ctx).await.unwrap_err();
        assert!(err.contains("syntax errors"), "{err}");
        assert!(ctx.pending_edits.list().is_empty());
    }

    #[test]
    fn parse_lang_rejects_unknown() {
        assert!(parse_lang("brainfuck").is_err());
    }
}

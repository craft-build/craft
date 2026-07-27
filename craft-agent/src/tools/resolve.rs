//! Commits or discards a staged `ast_edit` proposal by `edit_id`.
//!
//! `resolve(edit_id, "apply")` verifies each staged file is unchanged on disk since the
//! preview (re-reading the current content and comparing to the staged `before`), writes the
//! new content, and pushes a backup to the safety snapshot store so `safety undo` can roll it
//! back. A file that changed since the preview aborts the apply with a stale-read error so the
//! model re-runs `ast_edit`; a write failure is logged and that file is skipped. The pending
//! proposal is consumed on a full apply.
//! `resolve(edit_id, "discard")` drops the proposal without writing anything.

use std::path::Path;

use craft_tool_macro::Tool;
use serde::Deserialize;
use tracing::warn;

use crate::ToolOutput;

use super::ast_edit::PendingEdit;

const UNKNOWN_EDIT: &str = "unknown edit_id; no pending ast_edit matches";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Resolve {
    #[param(description = "edit_id returned by a prior ast_edit call")]
    edit_id: String,
    #[param(description = "\"apply\" to commit the staged edit, or \"discard\" to drop it")]
    action: String,
}

impl Resolve {
    pub const NAME: &str = "resolve";
    pub const DESCRIPTION: &str = include_str!("resolve.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"edit_id": "e1a2b3c4", "action": "apply"},
  {"edit_id": "e1a2b3c4", "action": "discard"}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        match self.action.as_str() {
            "discard" => {
                if ctx.pending_edits.discard(&self.edit_id) {
                    Ok(ToolOutput::Plain(format!(
                        "discarded pending edit {}",
                        self.edit_id
                    )))
                } else {
                    Err(UNKNOWN_EDIT.into())
                }
            }
            "apply" => {
                let pending = ctx
                    .pending_edits
                    .take(&self.edit_id)
                    .ok_or_else(|| UNKNOWN_EDIT.to_string())?;
                self.apply(ctx, pending).await
            }
            other => Err(format!(
                "unknown action \"{other}\"; use \"apply\" or \"discard\""
            )),
        }
    }

    async fn apply(
        &self,
        ctx: &super::ToolContext,
        pending: PendingEdit,
    ) -> Result<ToolOutput, String> {
        let mut written = 0usize;
        let mut skipped = 0usize;
        for file in &pending.files {
            let rel = relative_path_lossy(&file.path);
            ctx.check_before_edit(&file.path)
                .map_err(|e| format!("{rel}: stale read; re-read then re-run ast_edit: {e}"))?;
            let current = ctx
                .fs
                .read_text_file(&file.path)
                .await
                .map_err(|e| format!("{rel}: cannot verify freshness before apply: {e}"))?;
            if current != file.before {
                return Err(format!(
                    "{rel}: file changed since preview; re-read and re-run ast_edit"
                ));
            }
            match ctx.fs.write_text_file(&file.path, &file.after).await {
                Ok(()) => {
                    ctx.snapshot_store
                        .push_backup(file.path.clone(), file.before.clone());
                    ctx.file_tracker.record_read(&file.path);
                    written += 1;
                }
                Err(e) => {
                    warn!(path = %file.path.display(), error = %e, "resolve: write failed");
                    skipped += 1;
                }
            }
        }
        if skipped > 0 {
            return Ok(ToolOutput::Plain(format!(
                "applied {written}/{} file(s) for edit {} ({} skipped); {repl} replacement(s)",
                pending.files.len(),
                self.edit_id,
                skipped,
                repl = pending.replacements
            )));
        }
        Ok(ToolOutput::Plain(format!(
            "applied {written} file(s) for edit {}; {} replacement(s)",
            self.edit_id, pending.replacements
        )))
    }

    pub fn start_header(&self) -> String {
        format!("resolve {} {}", self.edit_id, self.action)
    }
}

fn relative_path_lossy(p: &Path) -> String {
    super::relative_path(&p.to_string_lossy())
}

super::impl_tool!(
    Resolve,
    audience = super::ToolAudience::MAIN,
    kind = "resolve",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for Resolve {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Resolve::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        None
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Resolve::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::ast_edit::AstEdit;
    use crate::tools::test_support::stub_ctx;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    async fn stage(ctx: &super::super::ToolContext, dir: &TempDir) -> String {
        let path = dir.path().join("a.rs");
        fs::write(&path, "Vec::new();\n").unwrap();
        let edit = AstEdit::parse_input(&json!({
            "pattern": "Vec::new()",
            "rewrite": "vec![]",
            "lang": "rust",
            "path": dir.path().to_str().unwrap(),
        }))
        .unwrap();
        edit.execute(ctx).await.unwrap();
        ctx.pending_edits.list()[0].edit_id.clone()
    }

    #[tokio::test]
    async fn apply_writes_files() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        let ctx = stub_ctx(&AgentMode::Build);
        let id = stage(&ctx, &dir).await;
        let resolve = Resolve::parse_input(&json!({"edit_id": id, "action": "apply"})).unwrap();
        let out = resolve.execute(&ctx).await.unwrap();
        assert!(
            out.as_text().contains("applied 1 file"),
            "{}",
            out.as_text()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "vec![];\n");
        assert!(ctx.pending_edits.list().is_empty());
    }

    #[tokio::test]
    async fn discard_leaves_tree_untouched() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        let ctx = stub_ctx(&AgentMode::Build);
        let id = stage(&ctx, &dir).await;
        let resolve = Resolve::parse_input(&json!({"edit_id": id, "action": "discard"})).unwrap();
        resolve.execute(&ctx).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "Vec::new();\n");
        assert!(ctx.pending_edits.list().is_empty());
    }

    #[tokio::test]
    async fn unknown_edit_id_errors() {
        let ctx = stub_ctx(&AgentMode::Build);
        let resolve = Resolve::parse_input(&json!({"edit_id": "nope", "action": "apply"})).unwrap();
        let err = resolve.execute(&ctx).await.unwrap_err();
        assert!(err.contains(UNKNOWN_EDIT), "{err}");
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let ctx = stub_ctx(&AgentMode::Build);
        ctx.pending_edits
            .stage(crate::tools::ast_edit::PendingEdit {
                edit_id: "e1".into(),
                pattern: "x".into(),
                lang: "rust".into(),
                replacements: 0,
                files: vec![],
            })
            .unwrap();
        let resolve =
            Resolve::parse_input(&json!({"edit_id": "e1", "action": "frobnicate"})).unwrap();
        let err = resolve.execute(&ctx).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");
    }

    #[tokio::test]
    async fn apply_pushes_backup_to_snapshot_store() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        let ctx = stub_ctx(&AgentMode::Build);
        let id = stage(&ctx, &dir).await;
        let resolve = Resolve::parse_input(&json!({"edit_id": id, "action": "apply"})).unwrap();
        resolve.execute(&ctx).await.unwrap();
        let guard = ctx.snapshot_store.0.lock().unwrap();
        assert!(guard.backups.contains_key(&path), "backup should exist");
    }
}

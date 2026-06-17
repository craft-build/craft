use std::path::{Path, PathBuf};

use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Delete {
    #[param(description = "Files or directories to delete")]
    files: Vec<String>,
    #[param(description = "Delete directories recursively (required for non-empty dirs)")]
    recursive: Option<bool>,
}

impl Delete {
    pub const NAME: &str = "delete";
    pub const DESCRIPTION: &str = include_str!("delete.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"files": ["src/old_module.rs"]}, {"files": ["build/"], "recursive": true}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let recursive = self.recursive.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();

        for file in &self.files {
            let path =
                super::resolve_path(file).map_err(|e| format!("invalid path \"{file}\": {e}"))?;
            let path = PathBuf::from(&path);

            if !path.exists() {
                skipped.push(format!(
                    "{} (not found)",
                    relative_path(&path.to_string_lossy())
                ));
                continue;
            }

            if path.is_dir() && !recursive {
                skipped.push(format!(
                    "{} (is a directory; set recursive=true)",
                    relative_path(&path.to_string_lossy())
                ));
                continue;
            }

            ctx.file_tracker.check_before_edit(&path)?;
            let rel = relative_path(&path.to_string_lossy());

            if path.is_file() {
                if let Ok(content) = ctx.fs.read_text_file(&path).await {
                    ctx.snapshot_store.push_backup(path.clone(), content);
                }
                let p = path.clone();
                let res = tokio::task::spawn_blocking(move || std::fs::remove_file(&p))
                    .await
                    .map_err(|e| format!("delete task failed: {e}"))?;
                match res {
                    Ok(()) => deleted.push(rel),
                    Err(e) => skipped.push(format!("{rel} ({e})")),
                }
            } else if path.is_dir() {
                let p = path.clone();
                let res = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&p))
                    .await
                    .map_err(|e| format!("delete task failed: {e}"))?;
                match res {
                    Ok(()) => deleted.push(rel),
                    Err(e) => skipped.push(format!("{rel} ({e})")),
                }
            } else {
                skipped.push(format!("{rel} (special file, skipped)"));
            }
        }

        if deleted.is_empty() && !skipped.is_empty() {
            return Err(format!("skipped: {}", skipped.join(", ")));
        }

        let mut out = String::new();
        if !deleted.is_empty() {
            out.push_str(&format!("deleted: {}", deleted.join(", ")));
        }
        if !skipped.is_empty() {
            out.push_str(&format!("\nskipped: {}", skipped.join(", ")));
        }

        Ok(ToolOutput::Plain(out))
    }

    pub fn start_header(&self) -> String {
        format!("delete {}", self.files.join(", "))
    }
}

super::impl_tool!(
    Delete,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "delete",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for Delete {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Delete::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        self.files.first().map(Path::new)
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let ctx = crate::types::PermissionContext {
            files: self.files.clone(),
            commands: vec![],
            reason: Some("delete files".into()),
        };
        let scopes = self
            .files
            .iter()
            .map(|f| crate::permissions::canonicalize_scope_path(f))
            .collect();
        Box::pin(std::future::ready(Some(
            super::PermissionScopes::multiple_with_context(scopes, ctx),
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Delete::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    #[tokio::test]
    async fn delete_file_removes_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "hello").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let ctx = stub_ctx(&AgentMode::Build);
        let out = Delete {
            files: vec![path_str],
            recursive: None,
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert!(!path.exists());
        assert!(out.as_text().contains("deleted"));
        let guard = ctx.snapshot_store.0.lock().unwrap();
        assert!(guard.backups.contains_key(&path));
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");

        let ctx = stub_ctx(&AgentMode::Build);
        let result = Delete {
            files: vec![missing.to_string_lossy().to_string()],
            recursive: None,
        }
        .execute(&ctx)
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        const EXPECTED: &str = "not found";
        assert!(err.contains(EXPECTED), "got: {err}");
    }
}

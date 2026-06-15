use std::path::Path;

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::fuzzy_replace;
use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Edit {
    #[param(description = "Absolute path to the file", alias = "file_path")]
    path: String,
    #[param(description = "Exact string to find (must match uniquely unless replace_all is true)")]
    old_string: String,
    #[param(description = "Replacement string")]
    new_string: String,
    #[param(description = "Replace all occurrences (default false)")]
    replace_all: Option<bool>,
}

impl Edit {
    pub const NAME: &str = "edit";
    pub const DESCRIPTION: &str = include_str!("edit.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs", "old_string": "fn old_name(", "new_string": "fn new_name("},
  {"path": "/project/src/lib.rs", "old_string": "use std::collections::HashMap;\nuse std::sync::Arc;", "new_string": "use std::collections::HashMap;\nuse std::io::Read;\nuse std::sync::Arc;"}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let p = Path::new(&path);
        ctx.file_tracker.check_before_edit(p)?;

        let before = ctx.fs.read_text_file(p).await?;
        let after = fuzzy_replace::replace(
            &before,
            &self.old_string,
            &self.new_string,
            self.replace_all.unwrap_or(false),
        )?;
        ctx.fs.write_text_file(p, &after).await?;

        ctx.file_tracker.record_read(p);

        Ok(ToolOutput::Diff {
            summary: format!("edited {}", relative_path(&path)),
            path,
            before,
            after,
        })
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(
    Edit,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "edit",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for Edit {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Edit::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let ctx = crate::types::PermissionContext {
            files: vec![self.path.clone()],
            commands: vec![],
            reason: Some("edit file".into()),
        };
        Box::pin(std::future::ready(Some(super::PermissionScopes::single_with_context(
            crate::permissions::canonicalize_scope_path(&self.path),
            ctx,
        ))))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Edit::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::AgentMode;
    use crate::tools::test_support::{pre_read, stub_ctx};

    use super::*;

    fn temp_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn edit_reads_replaces_writes() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);

        let path = temp_file(&dir, "f.rs", "fn old() {}\nfn keep() {}");
        pre_read(&ctx, &path);
        Edit {
            path: path.clone(),
            old_string: "fn old() {}".into(),
            new_string: "fn new() {}".into(),
            replace_all: None,
        }
        .execute(&ctx)
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "fn new() {}\nfn keep() {}"
        );

        let path = temp_file(&dir, "g.rs", "let x = 1;\nlet x = 1;\nlet y = 2;");
        pre_read(&ctx, &path);
        Edit {
            path: path.clone(),
            old_string: "let x = 1;".into(),
            new_string: "let x = 9;".into(),
            replace_all: Some(true),
        }
        .execute(&ctx)
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "let x = 9;\nlet x = 9;\nlet y = 2;"
        );
    }

    /// Regression: when `old_string` contains a literal `\n` it is unescaped
    /// before writing, so the Diff snapshots must be the post-unescape file
    /// content, not the raw single-line input. This is the structural
    /// guarantee that prevents the old "diff full of `\n` escapes" bug.
    #[tokio::test]
    async fn diff_snapshots_are_real_file_content_not_raw_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let original = "const A: u8 = 1;\nconst B: u8 = 2;\n";
        let updated = "const A: u8 = 9;\nconst B: u8 = 2;\n";
        let path = temp_file(&dir, "f.rs", original);
        pre_read(&ctx, &path);
        let output = Edit {
            path: path.clone(),
            old_string: "const A: u8 = 1;\\nconst B: u8 = 2;".into(),
            new_string: "const A: u8 = 9;\\nconst B: u8 = 2;".into(),
            replace_all: None,
        }
        .execute(&ctx)
        .await
        .unwrap();

        let ToolOutput::Diff { before, after, .. } = output else {
            panic!("expected Diff output");
        };
        assert_eq!(before, original);
        assert_eq!(after, updated);
        assert_eq!(fs::read_to_string(&path).unwrap(), updated);
    }
}


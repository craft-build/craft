use std::path::Path;

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Write {
    #[param(description = "Absolute path to the file", alias = "file_path")]
    path: String,
    #[param(description = "The complete file content to write")]
    content: String,
}

impl Write {
    pub const NAME: &str = "write";
    pub const DESCRIPTION: &str = include_str!("write.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"path": "/project/src/config.rs", "content": "pub const PORT: u16 = 8080;\n"}]"#);

    fn write_output(&self, resolved_path: &str, max_lines: usize) -> ToolOutput {
        ToolOutput::WriteCode {
            path: resolved_path.to_owned(),
            byte_count: self.content.len(),
            lines: self
                .content
                .lines()
                .take(max_lines)
                .map(ToOwned::to_owned)
                .collect(),
        }
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let output = self.write_output(&path, ctx.config.max_output_lines);
        let p = Path::new(&path);
        ctx.file_tracker.check_before_edit(p)?;
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir error: {e}"))?;
        }
        ctx.fs.write_text_file(p, &self.content).await?;
        ctx.file_tracker.record_read(p);
        Ok(output)
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(
    Write,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "edit",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for Write {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Write::start_header(self)))
    }
    fn start_output(&self) -> Option<ToolOutput> {
        let path = super::resolve_path(&self.path).ok()?;
        Some(self.write_output(&path, craft_config::DEFAULT_MAX_OUTPUT_LINES))
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let ctx = crate::types::PermissionContext {
            files: vec![self.path.clone()],
            commands: vec![],
            reason: Some("write file".into()),
        };
        Box::pin(std::future::ready(Some(
            super::PermissionScopes::single_with_context(
                crate::permissions::canonicalize_scope_path(&self.path),
                ctx,
            ),
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Write::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use crate::AgentMode;
    use crate::tools::test_support::{pre_read, stub_ctx};

    use super::*;

    #[tokio::test]
    async fn write_existing_without_read_allowed() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = dir.path().join("existing.txt");
        fs::write(&path, "original").unwrap();

        Write {
            path: path.to_string_lossy().to_string(),
            content: "overwrite".into(),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "overwrite");
    }

    #[tokio::test]
    async fn write_existing_after_read_succeeds() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = dir.path().join("existing.txt");
        fs::write(&path, "original").unwrap();
        pre_read(&ctx, &path.to_string_lossy());

        Write {
            path: path.to_string_lossy().to_string(),
            content: "overwrite".into(),
        }
        .execute(&ctx)
        .await
        .unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "overwrite");
    }
}

use animist::agent::tool::{ToolContext, ToolImpl, ToolOutput};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::diff::unified_text;
use crate::harness::dedup;

use super::{
    MAX_FILE_BYTES, Workspace, claims, commit_staged,
    error::{Error, Result, definition, failure, finish, invalid, io_error},
    read_bytes, stage_bytes_for_write, text,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    pub path: String,
    /// Complete file content, replacing any existing content.
    pub content: String,
}

#[derive(Debug)]
pub struct WriteOutput {
    pub path: String,
    pub created: bool,
    pub bytes_written: usize,
    /// Previous content when overwriting; empty for created files.
    pub before: String,
    pub after: String,
}

impl super::error::IntoToolResult for WriteOutput {
    fn into_tool_result(self) -> ToolOutput {
        if self.created {
            // A full-file add-diff would just duplicate the content the model
            // already sent; a summary is enough.
            return super::error::text_output(format!(
                "wrote {} ({} bytes)",
                self.path, self.bytes_written
            ));
        }
        let summary = format!("overwrote {} ({} bytes)", self.path, self.bytes_written);
        super::error::text_output(unified_text(
            &self.before,
            &self.after,
            &summary,
            &self.path,
        ))
    }
}

#[derive(Clone)]
pub struct Write(pub Workspace);

impl Write {
    async fn execute(
        workspace: &Workspace,
        args: WriteArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<WriteOutput> {
        if args.content.contains('\0') {
            return Err(invalid("content must not contain NUL bytes"));
        }
        if args.content.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "written file would exceed the 8 MiB tool size limit",
            ));
        }
        let path = workspace.target(&args.path)?;
        // Cross-agent: the write guard spans resolve → staged write → persist
        // so a concurrent session cannot interleave with the lost-update check.
        let _guard = workspace
            .write_guard(std::slice::from_ref(&args.path), cancel)
            .await?;
        workspace.note_snapshot(&path).await;
        let existed = tokio::fs::symlink_metadata(&path)
            .await
            .map(|_| true)
            .or_else(|error| match error.kind() {
                std::io::ErrorKind::NotFound => Ok(false),
                _ => Err(io_error(error)),
            })?;
        let before = if existed {
            text(read_bytes(&path).await?)?
        } else {
            String::new()
        };
        let parent = path
            .parent()
            .ok_or_else(|| failure("resolved path has no parent directory"))?
            .to_path_buf();
        let staged = stage_bytes_for_write(&parent, args.content.as_bytes()).await?;
        if workspace.target(&args.path)? != path
            || tokio::fs::symlink_metadata(&path).await.is_ok() != existed
        {
            return Err(failure(
                "file changed while preparing the write; read it again and retry",
            ));
        }
        commit_staged(staged, &path).await?;
        Ok(WriteOutput {
            path: workspace.display(&path),
            created: !existed,
            bytes_written: args.content.len(),
            before,
            after: args.content,
        })
    }
}

#[async_trait]
impl ToolImpl for Write {
    type Args = WriteArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> animist::agent::tool::ToolDefinition {
        definition::<WriteArgs>(
            "write",
            "Write complete content to a UTF-8 file inside the workspace, replacing existing content. Prefer editing existing files; only create files when necessary and never proactively create documentation or README files. Parent directories are created if needed. Atomic, no symlinks, Git metadata, or paths outside the workspace.",
        )
    }

    fn scheduling(&self, input: &Value) -> animist::agent::tool::ToolScheduling {
        claims::writes(
            self.0.root_key().as_deref(),
            &dedup::extract_write_paths("write", input),
        )
    }

    async fn call(&self, context: &ToolContext<'_>, args: WriteArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

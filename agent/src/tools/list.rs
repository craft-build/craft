use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::error::{Error, Result, definition, finish, invalid, io_error};
use super::{Workspace, claims};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    /// Directory to list, relative to the workspace.
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    ".".into()
}

#[derive(Debug)]
pub struct ListOutput {
    /// Entry names, directories first with a trailing `/`.
    pub entries: Vec<String>,
    /// Subdirectory instruction files discovered for this directory, as
    /// `(canonical_path, content)` pairs.
    pub instructions: Vec<(String, String)>,
}

impl super::error::IntoToolResult for ListOutput {
    fn into_tool_result(self) -> ToolOutput {
        let mut text = if self.entries.is_empty() {
            "(empty directory)".to_string()
        } else {
            self.entries.join("\n")
        };
        for (path, content) in &self.instructions {
            text.push_str(&format!("\n\n---\nInstructions from: {path}\n{content}"));
        }
        super::error::text_output(text)
    }
}

#[derive(Clone)]
pub struct List(pub Workspace);

impl List {
    async fn execute(
        workspace: &Workspace,
        args: ListArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<ListOutput> {
        let path = workspace.resolve(&args.path)?;
        if !path.is_dir() {
            return Err(invalid("path must identify a directory"));
        }
        // Cross-agent: the root-level shared guard excludes only the
        // workspace-wide writer (move); file-level writers are ordered by
        // the scheduling claim through the directory key.
        let _guard = workspace.read_guard(&[], cancel).await?;
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut entries = tokio::fs::read_dir(&path).await.map_err(io_error)?;
        while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // tokio DirEntry::file_type is a future.
            if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                dirs.push(format!("{name}/"));
            } else {
                files.push(name);
            }
        }
        dirs.sort_unstable();
        files.sort_unstable();
        files.retain(|name| !crate::instructions::is_instruction_file(name));
        dirs.append(&mut files);
        let instructions = crate::instructions::find_subdirectory_instructions(
            &path,
            workspace.root(),
            &workspace.loaded_instructions,
        );
        Ok(ListOutput {
            entries: dirs,
            instructions,
        })
    }
}

#[async_trait]
impl ToolImpl for List {
    type Args = ListArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<ListArgs>(
            "list",
            "List directory contents. Entries are sorted alphabetically with directories first and a trailing '/'. Instruction files (AGENTS.md, CLAUDE.md, etc.) are filtered out. No symlinks, Git metadata, or paths outside the workspace.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        // A directory listing claims its directory: writers claim every
        // ancestor directory, so a listing conflicts with any write
        // beneath it.
        let fallback = default_path();
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(fallback.as_str());
        claims::reads(self.0.root_key().as_deref(), &[path.to_owned()])
    }

    async fn call(&self, context: &ToolContext<'_>, args: ListArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

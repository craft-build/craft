use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::run::dedup;

use super::edit::persist;
use super::error::{Error, Result, definition, finish, invalid};
use super::fuzzy_replace;
use super::{MAX_FILE_BYTES, Workspace, claims, read_bytes, text};

const SNIPPET_MAX_CHARS: usize = 32;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditEntry {
    /// Nonempty text to find; matched fuzzily like edit.
    pub old_string: String,
    pub new_string: String,
    /// Replace all non-overlapping matches. Otherwise one unique match is required.
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MultiEditArgs {
    pub path: String,
    /// Edit operations applied sequentially; each sees the previous edits' result.
    pub edits: Vec<EditEntry>,
}

#[derive(Debug)]
pub struct MultiEditOutput {
    pub path: String,
    pub edits: usize,
    pub bytes_written: usize,
}

impl super::error::IntoToolResult for MultiEditOutput {
    fn into_tool_result(self) -> ToolOutput {
        let plural = if self.edits == 1 { "" } else { "s" };
        super::error::text_output(format!(
            "applied {} edit{plural} to {}",
            self.edits, self.path
        ))
    }
}

#[derive(Clone)]
pub struct MultiEdit(pub Workspace);

impl MultiEdit {
    fn first_line_snippet(s: &str) -> String {
        let line = s.split('\n').next().unwrap_or("");
        let chars: Vec<char> = line.chars().take(SNIPPET_MAX_CHARS).collect();
        if line.chars().count() > SNIPPET_MAX_CHARS {
            format!("{}…", chars.into_iter().collect::<String>())
        } else {
            chars.into_iter().collect()
        }
    }

    fn edit_error(index: usize, edit: &EditEntry, reason: &str) -> Error {
        invalid(format!(
            "edits[{index}] (old_string {:?}): {reason}",
            Self::first_line_snippet(&edit.old_string)
        ))
    }

    fn apply_edit(content: &str, edit: &EditEntry) -> std::result::Result<String, String> {
        fuzzy_replace::replace(
            content,
            &edit.old_string,
            &edit.new_string,
            edit.replace_all,
            None,
        )
        .map(|r| r.content)
    }

    async fn execute(
        workspace: &Workspace,
        args: MultiEditArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<MultiEditOutput> {
        if args.edits.is_empty() {
            return Err(invalid("provide at least one edit"));
        }
        for edit in &args.edits {
            if edit.old_string.is_empty() {
                return Err(invalid(
                    "every old_string must be nonempty; multiedit only modifies existing files",
                ));
            }
            if edit.old_string == edit.new_string {
                return Err(invalid("every old_string and new_string must differ"));
            }
            if edit.new_string.contains('\0') {
                return Err(invalid("new_string must not contain NUL bytes"));
            }
        }
        let path = workspace.file(&args.path)?;
        let _guard = workspace
            .write_guard(std::slice::from_ref(&args.path), cancel)
            .await?;
        let before = text(read_bytes(&path).await?)?;
        let mut after = before.clone();
        for (index, edit) in args.edits.iter().enumerate() {
            after = Self::apply_edit(&after, edit)
                .map_err(|reason| Self::edit_error(index, edit, &reason))?;
        }
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "edited file would exceed the 8 MiB tool size limit",
            ));
        }
        let size = after.len();
        persist(workspace, &args.path, &path, &before, &after).await?;
        Ok(MultiEditOutput {
            path: workspace.display(&path),
            edits: args.edits.len(),
            bytes_written: size,
        })
    }
}

#[async_trait]
impl ToolImpl for MultiEdit {
    type Args = MultiEditArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<MultiEditArgs>(
            "multiedit",
            "Apply multiple find/replace edits to one existing UTF-8 file atomically and in sequence; each edit sees the result of the previous one. Matching is fuzzy-tolerant like edit: old_string must resolve to one match unless replace_all is set. Any failure leaves the file unchanged. No creation, symlinks, Git metadata, or paths outside the workspace.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        claims::writes(
            self.0.root_key().as_deref(),
            &dedup::extract_write_paths("multiedit", input),
        )
    }

    async fn call(&self, context: &ToolContext<'_>, args: MultiEditArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

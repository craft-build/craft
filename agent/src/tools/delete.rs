use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::run::dedup;

use super::error::{Error, Result, definition, finish, invalid, io_error};
use super::{Workspace, claims};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    /// Files or directories to delete.
    pub files: Vec<String>,
    /// Delete directories recursively (required for non-empty dirs).
    pub recursive: Option<bool>,
}

#[derive(Debug)]
pub struct DeleteOutput {
    pub deleted: Vec<String>,
    pub skipped: Vec<String>,
}

impl super::error::IntoToolResult for DeleteOutput {
    fn into_tool_result(self) -> ToolOutput {
        let mut out = String::new();
        if !self.deleted.is_empty() {
            out.push_str(&format!("deleted: {}", self.deleted.join(", ")));
        }
        if !self.skipped.is_empty() {
            out.push_str(&format!("\nskipped: {}", self.skipped.join(", ")));
        }
        super::error::text_output(out.trim_start_matches('\n'))
    }
}

#[derive(Clone)]
pub struct Delete(pub Workspace);

impl Delete {
    /// Whether any target is (or may be) a directory: a recursive delete
    /// removes files whose individual keys no file-level claim covers, so
    /// it must run exclusively. Missing paths are no-ops, not trees;
    /// unreadable ones fail closed.
    fn has_directory_target(workspace: &Workspace, files: &[String]) -> bool {
        files.iter().any(|file| match workspace.resolve(file) {
            Ok(path) => match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata.is_dir(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => true,
            },
            // Missing paths resolve to NotFound: the entry is a skip.
            Err(Error::Io {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => false,
            // Any other resolution failure fails closed.
            Err(_) => true,
        })
    }

    async fn execute(
        workspace: &Workspace,
        args: DeleteArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<DeleteOutput> {
        let recursive = args.recursive.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();
        // Cross-agent: one guard spans every entry, so the batch deletes as
        // a set against other sessions' writers. A directory target removes
        // files beneath it, so it takes the workspace-wide guard (matching
        // its exclusive scheduling claim).
        let tree_target = Self::has_directory_target(workspace, &args.files);
        // A tree delete holds the workspace-wide lock: it excludes every
        // reader and writer keyed under the workspace, matching the
        // exclusive scheduling claim. File deletes take per-key guards.
        let keys = claims::write_keys(workspace.root_key().as_deref(), &args.files);
        let (_tree_guard, _file_guard) = if tree_target {
            (Some(workspace.locks().workspace_write(cancel).await?), None)
        } else {
            (None, Some(workspace.locks().write(&keys, cancel).await?))
        };

        for file in &args.files {
            // Our guard set stays: resolve rejects `..`, outside paths, Git
            // metadata, and every symlink component. Missing paths are a
            // per-entry skip, not a failure.
            let path = match workspace.resolve(file) {
                Ok(path) => path,
                Err(Error::Io {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                }) => {
                    skipped.push(format!("{file} (not found)"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Refuse the workspace root itself: `delete {files: ["."]}`
            // passes the containment guard yet would erase the project.
            if path == *workspace.root() {
                return Err(invalid(format!(
                    "refusing to delete the workspace root ({})",
                    workspace.display(&path)
                )));
            }
            let display = workspace.display(&path);
            let metadata = match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    skipped.push(format!("{display} (not found)"));
                    continue;
                }
                Err(error) => return Err(io_error(error)),
            };

            if metadata.is_dir() && !recursive {
                skipped.push(format!("{display} (is a directory; set recursive=true)"));
                continue;
            }
            // The classification stats targets before the guard; a target
            // that became a directory in between would recurse under a
            // claim that named no write beneath it. Fail closed.
            if metadata.is_dir() && !tree_target {
                return Err(invalid(format!(
                    "{display} became a directory before the delete; retry"
                )));
            }

            // Capture pre-delete contents so `/undo` can restore them.
            if metadata.is_file() {
                workspace.note_snapshot(&path).await;
            } else if metadata.is_dir() {
                note_tree(workspace, &path).await;
            }

            let result = if metadata.is_file() {
                tokio::fs::remove_file(&path).await
            } else if metadata.is_dir() {
                tokio::fs::remove_dir_all(&path).await
            } else {
                skipped.push(format!("{display} (special file, skipped)"));
                continue;
            };
            match result {
                Ok(()) => deleted.push(display),
                Err(error) => skipped.push(format!("{display} ({error})")),
            }
        }

        if deleted.is_empty() && !skipped.is_empty() {
            let message = format!("skipped: {}", skipped.join(", "));
            return Err(if skipped.iter().all(|s| s.contains("(not found)")) {
                super::error::not_found(message)
            } else {
                invalid(message)
            });
        }

        Ok(DeleteOutput { deleted, skipped })
    }
}

/// Snapshot every file under `dir` (recursively) for `/undo`. Iterative so
/// deep trees cannot overflow the stack.
async fn note_tree(workspace: &Workspace, dir: &std::path::Path) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                workspace.note_snapshot(&path).await;
            }
        }
    }
}

#[async_trait]
impl ToolImpl for Delete {
    type Args = DeleteArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<DeleteArgs>(
            "delete",
            "Delete files or directories inside the workspace. Text file contents are captured for `/undo` before removal. Set `recursive=true` to remove non-empty directories. Non-existent paths and special files are skipped and reported; paths outside the workspace, symlinks, and Git metadata are refused.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        let paths = dedup::extract_write_paths("delete", input);
        // A directory target (or an unresolvable one, read as a tree) makes
        // the delete exclusive: file-level claims cannot cover the files a
        // recursive removal takes with it.
        if Self::has_directory_target(&self.0, &paths) {
            return ToolScheduling::Exclusive;
        }
        claims::writes(self.0.root_key().as_deref(), &paths)
    }

    async fn call(&self, context: &ToolContext<'_>, args: DeleteArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

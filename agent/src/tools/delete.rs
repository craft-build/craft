use std::fs;

use rig_core::tool::{IntoToolOutput, ToolErrorKind, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, Workspace, failure, impl_tool, invalid, io_error};

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

impl IntoToolOutput for DeleteOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut out = String::new();
        if !self.deleted.is_empty() {
            out.push_str(&format!("deleted: {}", self.deleted.join(", ")));
        }
        if !self.skipped.is_empty() {
            out.push_str(&format!("\nskipped: {}", self.skipped.join(", ")));
        }
        Ok(ToolOutput::text(out.trim_start_matches('\n')))
    }
}

#[derive(Clone)]
pub struct Delete(pub Workspace);

impl Delete {
    fn execute(workspace: &Workspace, args: DeleteArgs) -> Result<DeleteOutput> {
        let recursive = args.recursive.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut skipped = Vec::new();

        for file in &args.files {
            // Our guard set stays: resolve rejects `..`, outside paths, Git
            // metadata, and every symlink component. Missing paths are a
            // per-entry skip, not a failure.
            let path = match workspace.resolve(file) {
                Ok(path) => path,
                Err(error) if error.kind() == ToolErrorKind::NotFound => {
                    skipped.push(format!("{file} (not found)"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let display = workspace.display(&path);
            let metadata = match fs::symlink_metadata(&path) {
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

            // Capture pre-delete contents so `/undo` can restore them.
            if metadata.is_file() {
                workspace.note_snapshot(&path);
            } else if metadata.is_dir() {
                note_tree(workspace, &path);
            }

            let result = if metadata.is_file() {
                fs::remove_file(&path)
            } else if metadata.is_dir() {
                fs::remove_dir_all(&path)
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
                super::not_found(message)
            } else {
                invalid(message)
            });
        }

        Ok(DeleteOutput { deleted, skipped })
    }
}

fn note_tree(workspace: &Workspace, dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            note_tree(workspace, &path);
        } else {
            workspace.note_snapshot(&path);
        }
    }
}

impl_tool!(
    Delete,
    DeleteArgs,
    DeleteOutput,
    "delete",
    "Delete files or directories inside the workspace. Text file contents are captured for `/undo` before removal. Set `recursive=true` to remove non-empty directories. Non-existent paths and special files are skipped and reported; paths outside the workspace, symlinks, and Git metadata are refused."
);

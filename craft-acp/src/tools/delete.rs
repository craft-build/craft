use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Result, Workspace, impl_tool, io_error};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    /// One existing regular file inside the workspace. No directories or symlinks.
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteOutput {
    pub path: String,
    pub deleted: bool,
}

#[derive(Clone)]
pub struct Delete(pub Workspace);

impl Delete {
    fn execute(workspace: &Workspace, args: DeleteArgs) -> Result<DeleteOutput> {
        let path = workspace.file(&args.path)?;
        std::fs::remove_file(&path).map_err(io_error)?;
        Ok(DeleteOutput {
            path: workspace.display(&path),
            deleted: true,
        })
    }
}

impl_tool!(
    Delete,
    DeleteArgs,
    DeleteOutput,
    "delete",
    "Permanently delete one existing regular file inside the workspace. This is destructive and has no undo. Directories, symlinks, Git metadata, and outside paths are refused. Missing files return not-found errors; deletion is never recursive."
);

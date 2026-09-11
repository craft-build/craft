use std::io::Write as _;

use rig::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{MAX_FILE_BYTES, Result, Workspace, failure, impl_tool, invalid, io_error};

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
}

impl IntoToolOutput for WriteOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(format!(
            "wrote {} ({} bytes)",
            self.path, self.bytes_written
        )))
    }
}

#[derive(Clone)]
pub struct Write(pub Workspace);

impl Write {
    fn execute(workspace: &Workspace, args: WriteArgs) -> Result<WriteOutput> {
        if args.content.contains('\0') {
            return Err(invalid("content must not contain NUL bytes"));
        }
        if args.content.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "written file would exceed the 8 MiB tool size limit",
            ));
        }
        let path = workspace.target(&args.path)?;
        let existed = path.exists();
        let mut staged =
            tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io_error)?;
        staged
            .write_all(args.content.as_bytes())
            .map_err(io_error)?;
        staged.as_file().sync_all().map_err(io_error)?;
        if workspace.target(&args.path)? != path || path.exists() != existed {
            return Err(failure(
                "file changed while preparing the write; read it again and retry",
            ));
        }
        staged
            .persist(&path)
            .map_err(|error| io_error(error.error))?;
        Ok(WriteOutput {
            path: workspace.display(&path),
            created: !existed,
            bytes_written: args.content.len(),
        })
    }
}

impl_tool!(
    Write,
    WriteArgs,
    WriteOutput,
    "write",
    "Write complete content to a UTF-8 file inside the workspace, replacing existing content. Prefer editing existing files; only create files when necessary and never proactively create documentation or README files. Parent directories are created if needed. Atomic, no symlinks, Git metadata, or paths outside the workspace."
);

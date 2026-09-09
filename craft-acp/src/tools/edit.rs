use std::{fs, io::Write};

use rig::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    MAX_FILE_BYTES, Result, Workspace, denied, failure, impl_tool, invalid, io_error, read_bytes,
    text,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub path: String,
    /// Nonempty exact text to replace. Read first; do not include line numbers.
    pub old_string: String,
    pub new_string: String,
    /// Replace all non-overlapping matches. Otherwise one unique match is required.
    #[serde(default)]
    pub replace_all: bool,
    /// Select a particular match, one-based. Cannot be combined with replace_all.
    pub occurrence: Option<usize>,
}

#[derive(Debug)]
pub struct EditOutput {
    pub path: String,
    pub replacements: usize,
    pub bytes_written: usize,
}

impl IntoToolOutput for EditOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(format!("edited {}", self.path)))
    }
}

#[derive(Clone)]
pub struct Edit(pub Workspace);

impl Edit {
    fn execute(workspace: &Workspace, args: EditArgs) -> Result<EditOutput> {
        if args.old_string.is_empty() {
            return Err(invalid(
                "old_string must not be empty; edit only modifies existing files",
            ));
        }
        if args.old_string == args.new_string {
            return Err(invalid("old_string and new_string are identical"));
        }
        if args.replace_all && args.occurrence.is_some() {
            return Err(invalid("replace_all and occurrence are mutually exclusive"));
        }
        if args.new_string.contains('\0') {
            return Err(invalid("new_string must not contain NUL bytes"));
        }
        let path = workspace.file(&args.path)?;
        let before = text(read_bytes(&path)?)?;
        let count = before.match_indices(&args.old_string).count();
        if count == 0 {
            return Err(invalid(
                "old_string was not found; read the current file and retry",
            ));
        }
        let replacements = if args.replace_all { count } else { 1 };
        let occurrence = match args.occurrence {
            Some(n) if n == 0 || n > count => {
                return Err(invalid(format!("occurrence must be between 1 and {count}")));
            }
            Some(n) => n,
            None if !args.replace_all && count > 1 => {
                return Err(invalid(format!(
                    "old_string matches {count} times; provide more context, occurrence, or replace_all"
                )));
            }
            None => 1,
        };
        let size = before
            .len()
            .checked_sub(args.old_string.len() * replacements)
            .and_then(|size| {
                args.new_string
                    .len()
                    .checked_mul(replacements)
                    .and_then(|extra| size.checked_add(extra))
            })
            .filter(|size| *size <= MAX_FILE_BYTES)
            .ok_or_else(|| invalid("edited file would exceed the 8 MiB tool size limit"))?;
        let after = if args.replace_all {
            before.replace(&args.old_string, &args.new_string)
        } else {
            let index = before
                .match_indices(&args.old_string)
                .nth(occurrence - 1)
                .unwrap()
                .0;
            let mut after = before.clone();
            after.replace_range(index..index + args.old_string.len(), &args.new_string);
            after
        };
        let permissions = fs::metadata(&path).map_err(io_error)?.permissions();
        if permissions.readonly() {
            return Err(denied("file is read-only"));
        }
        // Stage beside the destination for same-filesystem atomic replacement.
        // Preserve exact bytes outside the replacement, including CRLF and BOM.
        let mut staged =
            tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io_error)?;
        staged.write_all(after.as_bytes()).map_err(io_error)?;
        staged
            .as_file()
            .set_permissions(permissions)
            .map_err(io_error)?;
        staged.as_file().sync_all().map_err(io_error)?;
        if workspace.file(&args.path)? != path || read_bytes(&path)? != before.as_bytes() {
            return Err(failure(
                "file changed while preparing the edit; read it again and retry",
            ));
        }
        staged
            .persist(&path)
            .map_err(|error| io_error(error.error))?;
        Ok(EditOutput {
            path: workspace.display(&path),
            replacements,
            bytes_written: size,
        })
    }
}

impl_tool!(
    Edit,
    EditArgs,
    EditOutput,
    "edit",
    "Atomically replace exact text in an existing UTF-8 file. Read first and exclude line-number prefixes. old_string must be unique unless replace_all or a one-based occurrence is set. Matching is exact, not fuzzy; line endings are preserved. No creation, symlinks, Git metadata, or paths outside the workspace."
);

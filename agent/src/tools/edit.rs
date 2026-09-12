use std::{fs, io::Write, path::Path};

use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    MAX_FILE_BYTES, Result, Workspace, denied, failure, impl_tool, invalid, io_error, read_bytes,
    text,
};

const OUT_OF_RANGE: &str = "out of range";

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
        persist(workspace, &args.path, &path, &before, &after)?;
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

/// Stage `after` beside the destination and atomically replace `path` while
/// preserving permissions, unless the file changed while we prepared the edit.
pub(crate) fn persist(
    workspace: &Workspace,
    requested: &str,
    path: &Path,
    before: &str,
    after: &str,
) -> Result<()> {
    let permissions = fs::metadata(path).map_err(io_error)?.permissions();
    if permissions.readonly() {
        return Err(denied("file is read-only"));
    }
    // Stage beside the destination for same-filesystem atomic replacement.
    // Preserve exact bytes outside the replacement, including CRLF and BOM.
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io_error)?;
    staged.write_all(after.as_bytes()).map_err(io_error)?;
    staged
        .as_file()
        .set_permissions(permissions)
        .map_err(io_error)?;
    staged.as_file().sync_all().map_err(io_error)?;
    if workspace.file(requested)? != path || read_bytes(path)? != before.as_bytes() {
        return Err(failure(
            "file changed while preparing the edit; read it again and retry",
        ));
    }
    staged
        .persist(path)
        .map_err(|error| io_error(error.error))?;
    Ok(())
}

/// Split off one trailing newline so callers work on whole lines, and remember
/// whether to re-append it. An empty file yields one empty line.
fn body_lines(content: &str) -> (Vec<&str>, bool) {
    let trailing_newline = content.ends_with('\n');
    let body = content.strip_suffix('\n').unwrap_or(content);
    (body.split('\n').collect(), trailing_newline)
}

fn join_lines(lines: &[&str], trailing_newline: bool) -> String {
    let joined = lines.join("\n");
    if trailing_newline {
        format!("{joined}\n")
    } else {
        joined
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditLinesArgs {
    pub path: String,
    /// First line to replace, one-based, inclusive.
    pub start: usize,
    /// Last line to replace, inclusive. Must be at least start.
    pub end: usize,
    /// Replacement text. Empty deletes the range.
    pub new_string: String,
}

#[derive(Debug)]
pub struct EditLinesOutput {
    pub path: String,
    pub bytes_written: usize,
}

impl IntoToolOutput for EditLinesOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(format!("edited lines in {}", self.path)))
    }
}

#[derive(Clone)]
pub struct EditLines(pub Workspace);

impl EditLines {
    fn replace_lines(
        content: &str,
        start: usize,
        end: usize,
        new_string: &str,
    ) -> std::result::Result<String, String> {
        let (lines, trailing_newline) = body_lines(content);
        let count = lines.len();
        if start < 1 || start > count {
            return Err(format!("start line {start} {OUT_OF_RANGE} (1-{count})"));
        }
        if end < start || end > count {
            return Err(format!("end line {end} {OUT_OF_RANGE} ({start}-{count})"));
        }
        let mut result = Vec::with_capacity(count + new_string.matches('\n').count() + 2);
        result.extend_from_slice(&lines[..start - 1]);
        if !new_string.is_empty() {
            result.extend(new_string.split('\n'));
        }
        result.extend_from_slice(&lines[end..]);
        Ok(join_lines(&result, trailing_newline))
    }

    fn execute(workspace: &Workspace, args: EditLinesArgs) -> Result<EditLinesOutput> {
        if args.new_string.contains('\0') {
            return Err(invalid("new_string must not contain NUL bytes"));
        }
        let path = workspace.file(&args.path)?;
        let before = text(read_bytes(&path)?)?;
        let after = Self::replace_lines(&before, args.start, args.end, &args.new_string)
            .map_err(invalid)?;
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "edited file would exceed the 8 MiB tool size limit",
            ));
        }
        let size = after.len();
        persist(workspace, &args.path, &path, &before, &after)?;
        Ok(EditLinesOutput {
            path: workspace.display(&path),
            bytes_written: size,
        })
    }
}

impl_tool!(
    EditLines,
    EditLinesArgs,
    EditLinesOutput,
    "edit_lines",
    "Edit lines by number in an existing UTF-8 file. Replaces lines start..=end (one-based, inclusive) with new_string; empty new_string deletes the range. Read and grep show lines as '<nr>: <content>', so those numbers can be used directly. An empty file exposes one empty line 1. Out-of-range ranges are rejected before any write; a trailing newline is preserved. Atomic, no symlinks, Git metadata, or paths outside the workspace."
);

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InsertLinesArgs {
    pub path: String,
    /// Line number to insert after, one-based. Use 0 to insert at the top of the file.
    pub line: usize,
    /// Text to insert; may contain multiple lines.
    pub new_string: String,
}

#[derive(Debug)]
pub struct InsertLinesOutput {
    pub path: String,
    pub bytes_written: usize,
}

impl IntoToolOutput for InsertLinesOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(format!("inserted lines in {}", self.path)))
    }
}

#[derive(Clone)]
pub struct InsertLines(pub Workspace);

impl InsertLines {
    fn insert_lines(
        content: &str,
        after_line: usize,
        new_string: &str,
    ) -> std::result::Result<String, String> {
        let (lines, trailing_newline) = body_lines(content);
        let count = lines.len();
        if after_line > count {
            return Err(format!("line {after_line} {OUT_OF_RANGE} (0-{count})"));
        }
        let mut result = Vec::with_capacity(count + new_string.matches('\n').count() + 2);
        result.extend_from_slice(&lines[..after_line]);
        result.extend(new_string.split('\n'));
        result.extend_from_slice(&lines[after_line..]);
        Ok(join_lines(&result, trailing_newline))
    }

    fn execute(workspace: &Workspace, args: InsertLinesArgs) -> Result<InsertLinesOutput> {
        if args.new_string.is_empty() {
            return Err(invalid(
                "new_string must not be empty; use edit_lines to delete",
            ));
        }
        if args.new_string.contains('\0') {
            return Err(invalid("new_string must not contain NUL bytes"));
        }
        let path = workspace.file(&args.path)?;
        let before = text(read_bytes(&path)?)?;
        let after = Self::insert_lines(&before, args.line, &args.new_string).map_err(invalid)?;
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "edited file would exceed the 8 MiB tool size limit",
            ));
        }
        let size = after.len();
        persist(workspace, &args.path, &path, &before, &after)?;
        Ok(InsertLinesOutput {
            path: workspace.display(&path),
            bytes_written: size,
        })
    }
}

impl_tool!(
    InsertLines,
    InsertLinesArgs,
    InsertLinesOutput,
    "insert_lines",
    "Insert new_string after a one-based line number in an existing UTF-8 file; 0 inserts at the top, and the last line number appends at the end. Only pass new lines, never lines already in the file. An empty file exposes one empty line 1. Out-of-range line numbers are rejected before any write; a trailing newline is preserved. Atomic, no symlinks, Git metadata, or paths outside the workspace."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_lines_replaces_and_deletes_ranges() {
        let content = "aaa\nbbb\nccc\nddd\neee\n";
        assert_eq!(
            EditLines::replace_lines(content, 2, 4, "XXX\nYYY").unwrap(),
            "aaa\nXXX\nYYY\neee\n"
        );
        assert_eq!(
            EditLines::replace_lines(content, 2, 3, "").unwrap(),
            "aaa\nddd\neee\n"
        );
        assert!(
            EditLines::replace_lines(content, 2, 6, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
    }

    #[test]
    fn replace_lines_handles_empty_and_no_trailing_newline() {
        assert_eq!(EditLines::replace_lines("", 1, 1, "seed").unwrap(), "seed");
        assert_eq!(
            EditLines::replace_lines("aaa\nbbb\nccc", 2, 2, "BBB").unwrap(),
            "aaa\nBBB\nccc"
        );
    }

    #[test]
    fn insert_lines_anchors_top_middle_end_and_empty_file() {
        assert_eq!(
            InsertLines::insert_lines("aaa\nbbb\nccc\n", 0, "TOP").unwrap(),
            "TOP\naaa\nbbb\nccc\n"
        );
        assert_eq!(
            InsertLines::insert_lines("aaa\nbbb\nccc\n", 3, "END").unwrap(),
            "aaa\nbbb\nccc\nEND\n"
        );
        assert_eq!(
            InsertLines::insert_lines("", 0, "seed\nmore").unwrap(),
            "seed\nmore\n"
        );
        assert!(
            InsertLines::insert_lines("aaa\n", 2, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
    }
}

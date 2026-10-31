use std::path::Path;

use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::diff::unified_text;
use crate::run::dedup;

use super::error::{Error, Result, definition, denied, failure, finish, invalid, io_error};
use super::fuzzy_replace;
use super::{MAX_FILE_BYTES, Workspace, claims, commit_staged, read_bytes, stage_bytes, text};

const OUT_OF_RANGE: &str = "out of range";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    pub path: String,
    /// Nonempty text to replace; matched fuzzily (whitespace, indentation, Unicode, and escape differences are tolerated). Read first; do not include line numbers.
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
    /// " (fuzzy match pass N)" when a non-exact pass matched, else empty.
    pub pass: String,
    pub before: String,
    pub after: String,
}

impl super::error::IntoToolResult for EditOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(unified_text(
            &self.before,
            &self.after,
            &format!("edited {}{}", self.path, self.pass),
            &self.path,
        ))
    }
}

#[derive(Clone)]
pub struct Edit(pub Workspace);

impl Edit {
    async fn execute(
        workspace: &Workspace,
        args: EditArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<EditOutput> {
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
        let _guard = workspace
            .write_guard(std::slice::from_ref(&args.path), cancel)
            .await?;
        let before = text(read_bytes(&path).await?)?;
        // CPU-bound fuzzy matching over up to 8 MiB.
        let haystack = before.clone();
        let old_string = args.old_string.clone();
        let new_string = args.new_string.clone();
        let result = tokio::task::spawn_blocking(move || {
            fuzzy_replace::replace(
                &haystack,
                &old_string,
                &new_string,
                args.replace_all,
                args.occurrence,
            )
        })
        .await
        .map_err(|source| Error::Blocking {
            source: Box::new(source),
        })?
        .map_err(invalid)?;
        let after = result.content;
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "edited file would exceed the 8 MiB tool size limit",
            ));
        }
        // Count what the winning pass actually replaced; an exact-only recount
        // reports 0 when a fuzzy pass did the work.
        let replacements = result.replacements;
        let pass = if result.pass == fuzzy_replace::Pass::Exact {
            String::new()
        } else {
            format!(" (fuzzy match pass {})", result.pass.number())
        };
        let size = after.len();
        persist(workspace, &args.path, &path, &before, &after).await?;
        Ok(EditOutput {
            path: workspace.display(&path),
            replacements,
            bytes_written: size,
            pass,
            before,
            after,
        })
    }
}

#[async_trait]
impl ToolImpl for Edit {
    type Args = EditArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<EditArgs>(
            "edit",
            "Atomically replace text in an existing UTF-8 file. Read first and exclude line-number prefixes. Matching tolerates drift: trailing whitespace, indentation, collapsed whitespace, Unicode, and escaped characters, plus fuzzy block anchoring. old_string must resolve to one match unless replace_all or a one-based occurrence is set. No creation, symlinks, Git metadata, or paths outside the workspace.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        claims::writes(
            self.0.root_key().as_deref(),
            &dedup::extract_write_paths("edit", input),
        )
    }

    async fn call(&self, context: &ToolContext<'_>, args: EditArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

/// Stage `after` beside the destination and atomically replace `path` while
/// preserving permissions, unless the file changed while we prepared the edit.
/// Callers hold the workspace path lock for the whole read-modify-write.
pub(crate) async fn persist(
    workspace: &Workspace,
    requested: &str,
    path: &Path,
    before: &str,
    after: &str,
) -> Result<()> {
    let permissions = tokio::fs::metadata(path)
        .await
        .map_err(io_error)?
        .permissions();
    if permissions.readonly() {
        return Err(denied("file is read-only"));
    }
    workspace.note_snapshot(path).await;
    // Stage beside the destination for same-filesystem atomic replacement.
    // Preserve exact bytes outside the replacement, including CRLF and BOM.
    let parent = path
        .parent()
        .ok_or_else(|| failure("resolved path has no parent directory"))?;
    let staged = stage_bytes(parent, after.as_bytes(), &permissions).await?;
    if workspace.file(requested)? != path || read_bytes(path).await? != before.as_bytes() {
        return Err(failure(
            "file changed while preparing the edit; read it again and retry",
        ));
    }
    commit_staged(staged, path).await?;
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
    pub before: String,
    pub after: String,
}

impl super::error::IntoToolResult for EditLinesOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(unified_text(
            &self.before,
            &self.after,
            &format!("edited lines in {}", self.path),
            &self.path,
        ))
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

    async fn execute(
        workspace: &Workspace,
        args: EditLinesArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<EditLinesOutput> {
        if args.new_string.contains('\0') {
            return Err(invalid("new_string must not contain NUL bytes"));
        }
        let path = workspace.file(&args.path)?;
        let _guard = workspace
            .write_guard(std::slice::from_ref(&args.path), cancel)
            .await?;
        let before = text(read_bytes(&path).await?)?;
        let after = Self::replace_lines(&before, args.start, args.end, &args.new_string)
            .map_err(invalid)?;
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "edited file would exceed the 8 MiB tool size limit",
            ));
        }
        let size = after.len();
        persist(workspace, &args.path, &path, &before, &after).await?;
        Ok(EditLinesOutput {
            path: workspace.display(&path),
            bytes_written: size,
            before,
            after,
        })
    }
}

#[async_trait]
impl ToolImpl for EditLines {
    type Args = EditLinesArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<EditLinesArgs>(
            "edit_lines",
            "Edit lines by number in an existing UTF-8 file. Replaces lines start..=end (one-based, inclusive) with new_string; empty new_string deletes the range. Read and grep show lines as '<nr>: <content>', so those numbers can be used directly. An empty file exposes one empty line 1. Out-of-range ranges are rejected before any write; a trailing newline is preserved. Atomic, no symlinks, Git metadata, or paths outside the workspace.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        claims::writes(
            self.0.root_key().as_deref(),
            &dedup::extract_write_paths("edit_lines", input),
        )
    }

    async fn call(&self, context: &ToolContext<'_>, args: EditLinesArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

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
    pub before: String,
    pub after: String,
}

impl super::error::IntoToolResult for InsertLinesOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(unified_text(
            &self.before,
            &self.after,
            &format!("inserted lines in {}", self.path),
            &self.path,
        ))
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

    async fn execute(
        workspace: &Workspace,
        args: InsertLinesArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<InsertLinesOutput> {
        if args.new_string.is_empty() {
            return Err(invalid(
                "new_string must not be empty; use edit_lines to delete",
            ));
        }
        if args.new_string.contains('\0') {
            return Err(invalid("new_string must not contain NUL bytes"));
        }
        let path = workspace.file(&args.path)?;
        let _guard = workspace
            .write_guard(std::slice::from_ref(&args.path), cancel)
            .await?;
        let before = text(read_bytes(&path).await?)?;
        let after = Self::insert_lines(&before, args.line, &args.new_string).map_err(invalid)?;
        if after.len() > MAX_FILE_BYTES {
            return Err(invalid(
                "edited file would exceed the 8 MiB tool size limit",
            ));
        }
        let size = after.len();
        persist(workspace, &args.path, &path, &before, &after).await?;
        Ok(InsertLinesOutput {
            path: workspace.display(&path),
            bytes_written: size,
            before,
            after,
        })
    }
}

#[async_trait]
impl ToolImpl for InsertLines {
    type Args = InsertLinesArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<InsertLinesArgs>(
            "insert_lines",
            "Insert new_string after a one-based line number in an existing UTF-8 file; 0 inserts at the top, and the last line number appends at the end. Only pass new lines, never lines already in the file. An empty file exposes one empty line 1. Out-of-range line numbers are rejected before any write; a trailing newline is preserved. Atomic, no symlinks, Git metadata, or paths outside the workspace.",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        claims::writes(
            self.0.root_key().as_deref(),
            &dedup::extract_write_paths("insert_lines", input),
        )
    }

    async fn call(&self, context: &ToolContext<'_>, args: InsertLinesArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

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

    #[tokio::test]
    async fn fuzzy_replace_all_reports_actual_count() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join("f.txt"),
            "let   x  =   1;\nlet   x  =   1;\n",
        )
        .unwrap();
        let cancel = animist::agent::Cancellation::default();
        let out = Edit::execute(
            &workspace,
            EditArgs {
                path: "f.txt".into(),
                old_string: "let x = 1;".into(),
                new_string: "let y = 2;".into(),
                replace_all: true,
                occurrence: None,
            },
            &cancel,
        )
        .await
        .unwrap();
        // An exact recount of old_string would report 0; the fuzzy pass
        // replaced both lines.
        assert!(!out.pass.is_empty());
        assert_eq!(out.replacements, 2);
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

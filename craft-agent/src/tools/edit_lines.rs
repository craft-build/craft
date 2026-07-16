use std::path::Path;

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::relative_path;

const OUT_OF_RANGE: &str = "out of range";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct EditLines {
    #[param(description = "Absolute path to the file", alias = "file_path")]
    path: String,
    #[param(description = "First line (1-indexed)")]
    start: usize,
    #[param(description = "Last line, inclusive")]
    end: usize,
    #[param(description = "Replacement text. Empty deletes the range.")]
    new_string: String,
}

impl EditLines {
    pub const NAME: &str = "edit_lines";
    pub const DESCRIPTION: &str = include_str!("edit_lines.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs", "start": 12, "end": 14, "new_string": "// replaced block"},
  {"path": "/project/src/lib.rs", "start": 5, "end": 5, "new_string": ""}
]"#,
    );

    fn replace_lines(
        content: &str,
        start: usize,
        end: usize,
        new_string: &str,
    ) -> Result<String, String> {
        let trailing_nl = content.ends_with('\n');
        let body = content.strip_suffix('\n').unwrap_or(content);
        let lines: Vec<&str> = body.split('\n').collect();
        let count = lines.len();

        if start < 1 || start > count {
            return Err(format!("start line {start} {OUT_OF_RANGE} (1-{count})"));
        }
        if end < start || end > count {
            return Err(format!("end line {end} {OUT_OF_RANGE} ({start}-{count})"));
        }

        let mut result: Vec<&str> =
            Vec::with_capacity(count + new_string.matches('\n').count() + 2);
        for &line in &lines[..start - 1] {
            result.push(line);
        }
        if !new_string.is_empty() {
            for line in new_string.split('\n') {
                result.push(line);
            }
        }
        for &line in &lines[end..] {
            result.push(line);
        }

        let joined = result.join("\n");
        Ok(if trailing_nl {
            format!("{joined}\n")
        } else {
            joined
        })
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let p = Path::new(&path);
        ctx.file_tracker.check_before_edit(p)?;

        let before = ctx.fs.read_text_file(p).await?;
        let after = Self::replace_lines(&before, self.start, self.end, &self.new_string)?;

        let validation = super::validation::validate_edit(p, &before, &after);
        if validation.introduced_errors {
            return Err(format!(
                "edit_lines introduced {} syntax error(s); rolled back. Check the start/end range for correctness",
                validation.error_count
            ));
        }

        ctx.fs.write_text_file(p, &after).await?;
        ctx.file_tracker.record_read(p);

        let warn = if !validation.syntax_valid {
            format!(" [{} pre-existing error(s)]", validation.error_count)
        } else {
            String::new()
        };

        let summary = format!(
            "replaced lines {}-{} in {}{}",
            self.start,
            self.end,
            relative_path(&path),
            warn
        );

        Ok(ToolOutput::Diff {
            summary,
            path,
            before,
            after,
        })
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(
    EditLines,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "edit",
    tier = super::ToolTier::Extended,
);

impl super::ToolInvocation for EditLines {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(EditLines::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let ctx = crate::types::PermissionContext {
            files: vec![self.path.clone()],
            commands: vec![],
            reason: Some("edit file lines".into()),
        };
        Box::pin(std::future::ready(Some(
            super::PermissionScopes::single_with_context(
                crate::permissions::normalize_scope_path(&self.path),
                ctx,
            ),
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move {
            let path = super::resolve_path(&self.path).ok();
            let result: super::ToolExecResult = EditLines::execute(&self, ctx).await.into();
            result.with_written_path(path)
        })
    }
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct InsertLines {
    #[param(description = "Absolute path to the file", alias = "file_path")]
    path: String,
    #[param(description = "Line number to insert before (1-indexed). Use 1 to insert at the top.")]
    line: usize,
    #[param(description = "Text to insert")]
    new_string: String,
}

impl InsertLines {
    pub const NAME: &str = "insert_lines";
    pub const DESCRIPTION: &str = include_str!("insert_lines.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/lib.rs", "line": 5, "new_string": "// inserted before line 5"},
  {"path": "/project/notes.txt", "line": 1, "new_string": "top of file"}
]"#,
    );

    fn insert_lines(content: &str, line: usize, new_string: &str) -> Result<String, String> {
        let trailing_nl = content.ends_with('\n');
        let body = content.strip_suffix('\n').unwrap_or(content);
        let lines: Vec<&str> = body.split('\n').collect();
        let count = lines.len();

        if line < 1 || line > count + 1 {
            return Err(format!("line {line} {OUT_OF_RANGE} (1-{})", count + 1));
        }

        let mut result: Vec<&str> =
            Vec::with_capacity(count + new_string.matches('\n').count() + 2);
        for &l in &lines[..line - 1] {
            result.push(l);
        }
        for l in new_string.split('\n') {
            result.push(l);
        }
        for &l in &lines[line - 1..] {
            result.push(l);
        }

        let joined = result.join("\n");
        Ok(if trailing_nl {
            format!("{joined}\n")
        } else {
            joined
        })
    }

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let p = Path::new(&path);
        ctx.file_tracker.check_before_edit(p)?;

        let before = ctx.fs.read_text_file(p).await?;
        let after = Self::insert_lines(&before, self.line, &self.new_string)?;

        let validation = super::validation::validate_edit(p, &before, &after);
        if validation.introduced_errors {
            return Err(format!(
                "insert_lines introduced {} syntax error(s); rolled back. Check the line number for correctness",
                validation.error_count
            ));
        }

        ctx.fs.write_text_file(p, &after).await?;
        ctx.file_tracker.record_read(p);

        let warn = if !validation.syntax_valid {
            format!(" [{} pre-existing error(s)]", validation.error_count)
        } else {
            String::new()
        };

        let summary = format!(
            "inserted at line {} in {}{}",
            self.line,
            relative_path(&path),
            warn
        );

        Ok(ToolOutput::Diff {
            summary,
            path,
            before,
            after,
        })
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(
    InsertLines,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "edit",
    tier = super::ToolTier::Extended,
);

impl super::ToolInvocation for InsertLines {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(InsertLines::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.path))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let ctx = crate::types::PermissionContext {
            files: vec![self.path.clone()],
            commands: vec![],
            reason: Some("insert file lines".into()),
        };
        Box::pin(std::future::ready(Some(
            super::PermissionScopes::single_with_context(
                crate::permissions::normalize_scope_path(&self.path),
                ctx,
            ),
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move {
            let path = super::resolve_path(&self.path).ok();
            let result: super::ToolExecResult = InsertLines::execute(&self, ctx).await.into();
            result.with_written_path(path)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use crate::AgentMode;
    use crate::tools::test_support::{pre_read, stub_ctx};

    use super::*;

    fn temp_file(dir: &TempDir, name: &str, content: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn replace_lines_range_replace_and_delete() {
        let content = "aaa\nbbb\nccc\nddd\neee\n";

        let r1 = EditLines::replace_lines(content, 2, 4, "XXX\nYYY").unwrap();
        assert_eq!(r1, "aaa\nXXX\nYYY\neee\n");

        let r2 = EditLines::replace_lines(content, 3, 3, "ZZZ").unwrap();
        assert_eq!(r2, "aaa\nbbb\nZZZ\nddd\neee\n");

        let r3 = EditLines::replace_lines(content, 2, 3, "").unwrap();
        assert_eq!(r3, "aaa\nddd\neee\n");

        assert!(
            EditLines::replace_lines(content, 0, 1, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
        assert!(
            EditLines::replace_lines(content, 2, 6, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
        assert!(
            EditLines::replace_lines(content, 3, 2, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
    }

    #[test]
    fn replace_lines_preserves_missing_trailing_newline() {
        let content = "aaa\nbbb\nccc";
        assert_eq!(
            EditLines::replace_lines(content, 2, 2, "BBB").unwrap(),
            "aaa\nBBB\nccc"
        );
    }

    #[test]
    fn insert_lines_basic() {
        let content = "aaa\nbbb\nccc\n";

        let r1 = InsertLines::insert_lines(content, 1, "ZZZ").unwrap();
        assert_eq!(r1, "ZZZ\naaa\nbbb\nccc\n");

        let r2 = InsertLines::insert_lines(content, 2, "XXX\nYYY").unwrap();
        assert_eq!(r2, "aaa\nXXX\nYYY\nbbb\nccc\n");

        let r3 = InsertLines::insert_lines(content, 4, "END").unwrap();
        assert_eq!(r3, "aaa\nbbb\nccc\nEND\n");

        let r4 = InsertLines::insert_lines(content, 2, "").unwrap();
        assert_eq!(r4, "aaa\n\nbbb\nccc\n");
    }

    #[test]
    fn insert_lines_out_of_range() {
        let content = "aaa\nbbb\nccc\n";

        assert!(
            InsertLines::insert_lines(content, 0, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
        assert!(
            InsertLines::insert_lines(content, 5, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
    }

    #[test]
    fn insert_lines_preserves_missing_trailing_newline() {
        let content = "aaa\nbbb\nccc";
        assert_eq!(
            InsertLines::insert_lines(content, 3, "INS").unwrap(),
            "aaa\nbbb\nINS\nccc"
        );
    }

    #[tokio::test]
    async fn execute_replaces_range_and_writes_file() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.rs", "aaa\nbbb\nccc\nddd\n");
        pre_read(&ctx, &path);
        let tool = EditLines::parse_input(&json!({
            "path": path,
            "start": 2,
            "end": 3,
            "new_string": "XXX\nYYY"
        }))
        .unwrap();
        tool.execute(&ctx).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nXXX\nYYY\nddd\n");
    }

    #[tokio::test]
    async fn execute_empty_new_string_deletes_range() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.rs", "aaa\nbbb\nccc\n");
        pre_read(&ctx, &path);
        let tool = EditLines::parse_input(&json!({
            "path": path,
            "start": 2,
            "end": 3,
            "new_string": ""
        }))
        .unwrap();
        tool.execute(&ctx).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\n");
    }

    #[tokio::test]
    async fn execute_out_of_range_leaves_file_unchanged() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let original = "aaa\nbbb\n";
        let path = temp_file(&dir, "f.rs", original);
        pre_read(&ctx, &path);
        let tool = EditLines::parse_input(&json!({
            "path": path,
            "start": 5,
            "end": 9,
            "new_string": "x"
        }))
        .unwrap();
        let err = tool.execute(&ctx).await.unwrap_err();
        assert!(err.contains(OUT_OF_RANGE), "got: {err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn execute_insert_lines_writes_file() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.rs", "aaa\nbbb\nccc\n");
        pre_read(&ctx, &path);
        let tool = InsertLines::parse_input(&json!({
            "path": path,
            "line": 2,
            "new_string": "INS"
        }))
        .unwrap();
        tool.execute(&ctx).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nINS\nbbb\nccc\n");
    }
}

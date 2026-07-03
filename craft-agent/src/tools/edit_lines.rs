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
    #[param(
        description = "Last line, inclusive. Omit to insert before start without removing lines."
    )]
    end: Option<usize>,
    #[param(description = "Replacement text. Empty deletes the range.")]
    new_string: String,
}

impl EditLines {
    pub const NAME: &str = "edit_lines";
    pub const DESCRIPTION: &str = include_str!("edit_lines.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs", "start": 12, "end": 14, "new_string": "// replaced block"},
  {"path": "/project/src/lib.rs", "start": 5, "new_string": "// inserted before line 5"}
]"#,
    );

    fn replace_lines(
        content: &str,
        start: usize,
        end: Option<usize>,
        new_string: &str,
    ) -> Result<String, String> {
        let trailing_nl = content.ends_with('\n');
        let body = content.strip_suffix('\n').unwrap_or(content);
        let lines: Vec<&str> = body.split('\n').collect();
        let count = lines.len();

        let (skip_from, skip_to) = match end {
            None => {
                if start < 1 || start > count + 1 {
                    return Err(format!(
                        "start line {start} {OUT_OF_RANGE} (1-{})",
                        count + 1
                    ));
                }
                (start, start - 1)
            }
            Some(end_line) => {
                if start < 1 || start > count {
                    return Err(format!("start line {start} {OUT_OF_RANGE} (1-{count})"));
                }
                if end_line < start || end_line > count {
                    return Err(format!(
                        "end line {end_line} {OUT_OF_RANGE} ({start}-{count})"
                    ));
                }
                (start, end_line)
            }
        };

        let mut result: Vec<&str> =
            Vec::with_capacity(count + new_string.matches('\n').count() + 2);
        for &line in &lines[..skip_from - 1] {
            result.push(line);
        }
        if !new_string.is_empty() {
            for line in new_string.split('\n') {
                result.push(line);
            }
        }
        for &line in &lines[skip_to..] {
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

        let summary = match self.end {
            Some(end_line) => format!(
                "replaced lines {}-{} in {}{}",
                self.start,
                end_line,
                relative_path(&path),
                warn
            ),
            None => format!(
                "inserted at line {} in {}{}",
                self.start,
                relative_path(&path),
                warn
            ),
        };

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

        let r1 = EditLines::replace_lines(content, 2, Some(4), "XXX\nYYY").unwrap();
        assert_eq!(r1, "aaa\nXXX\nYYY\neee\n");

        let r2 = EditLines::replace_lines(content, 3, Some(3), "ZZZ").unwrap();
        assert_eq!(r2, "aaa\nbbb\nZZZ\nddd\neee\n");

        let r3 = EditLines::replace_lines(content, 2, Some(3), "").unwrap();
        assert_eq!(r3, "aaa\nddd\neee\n");

        assert!(
            EditLines::replace_lines(content, 0, Some(1), "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
        assert!(
            EditLines::replace_lines(content, 2, Some(6), "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
        assert!(
            EditLines::replace_lines(content, 3, Some(2), "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
    }

    #[test]
    fn replace_lines_insert_mode() {
        let content = "aaa\nbbb\nccc\n";

        let r1 = EditLines::replace_lines(content, 1, None, "ZZZ").unwrap();
        assert_eq!(r1, "ZZZ\naaa\nbbb\nccc\n");

        let r2 = EditLines::replace_lines(content, 2, None, "XXX\nYYY").unwrap();
        assert_eq!(r2, "aaa\nXXX\nYYY\nbbb\nccc\n");

        let r3 = EditLines::replace_lines(content, 4, None, "END").unwrap();
        assert_eq!(r3, "aaa\nbbb\nccc\nEND\n");

        assert!(
            EditLines::replace_lines(content, 0, None, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
        assert!(
            EditLines::replace_lines(content, 5, None, "x")
                .unwrap_err()
                .contains(OUT_OF_RANGE)
        );
    }

    #[test]
    fn replace_lines_preserves_missing_trailing_newline() {
        let content = "aaa\nbbb\nccc";
        assert_eq!(
            EditLines::replace_lines(content, 2, Some(2), "BBB").unwrap(),
            "aaa\nBBB\nccc"
        );
        assert_eq!(
            EditLines::replace_lines(content, 3, None, "INS").unwrap(),
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
    async fn execute_insert_without_end_keeps_lines() {
        let dir = TempDir::new().unwrap();
        let ctx = stub_ctx(&AgentMode::Build);
        let path = temp_file(&dir, "f.rs", "aaa\nbbb\nccc\n");
        pre_read(&ctx, &path);
        let tool = EditLines::parse_input(&json!({
            "path": path,
            "start": 2,
            "new_string": "INS"
        }))
        .unwrap();
        tool.execute(&ctx).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nINS\nbbb\nccc\n");
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
}

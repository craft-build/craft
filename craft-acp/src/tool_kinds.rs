//! Map craft tool names and outputs to ACP `ToolKind` and `ToolCallContent`.
//!
//! Phase 3: every craft `ToolOutput` variant must render to a sequence of
//! ACP `ToolCallContent` values. Diff outputs use the structured `Diff`
//! content; everything else falls back to a text content block.

use agent_client_protocol::schema::Content;
use agent_client_protocol::schema::ContentBlock;
use agent_client_protocol::schema::Diff;
use agent_client_protocol::schema::ToolCallContent;
use agent_client_protocol::schema::ToolKind;
use craft_agent::ToolOutput;
use std::path::PathBuf;

const READ_TOOL_NAMES: &[&str] = &["read"];
const EDIT_TOOL_NAMES: &[&str] = &["write", "edit", "multiedit", "apply_patch"];
const SEARCH_TOOL_NAMES: &[&str] = &["grep", "glob", "index"];
const EXECUTE_TOOL_NAMES: &[&str] = &["bash", "code_execution"];
const FETCH_TOOL_NAMES: &[&str] = &["webfetch", "websearch"];
const THINK_TOOL_NAMES: &[&str] = &["task", "review", "check"];

/// Map a craft tool name to the closest ACP `ToolKind`.
pub fn tool_kind(name: &str) -> ToolKind {
    if READ_TOOL_NAMES.contains(&name) {
        ToolKind::Read
    } else if EDIT_TOOL_NAMES.contains(&name) {
        ToolKind::Edit
    } else if SEARCH_TOOL_NAMES.contains(&name) {
        ToolKind::Search
    } else if EXECUTE_TOOL_NAMES.contains(&name) {
        ToolKind::Execute
    } else if FETCH_TOOL_NAMES.contains(&name) {
        ToolKind::Fetch
    } else if THINK_TOOL_NAMES.contains(&name) {
        ToolKind::Think
    } else {
        ToolKind::Other
    }
}

/// Render a tool's output into ACP content blocks.
///
/// `Diff` outputs become structured `ToolCallContent::Diff`. `TodoList`
/// outputs are surfaced as plan notifications by `crate::plan` and emit no
/// inline content here. Everything else degrades to a single text block
/// using the same display text shown to humans.
pub fn render_content(output: &ToolOutput) -> Vec<ToolCallContent> {
    match output {
        ToolOutput::Diff {
            path, before, after, ..
        } => vec![ToolCallContent::Diff(
            Diff::new(PathBuf::from(path), after.clone()).old_text(Some(before.clone())),
        )],
        ToolOutput::TodoList(_) => Vec::new(),
        other => {
            let text = other.as_display_text();
            if text.is_empty() {
                Vec::new()
            } else {
                vec![ToolCallContent::Content(Content::new(ContentBlock::from(text)))]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("read", ToolKind::Read ; "read")]
    #[test_case("write", ToolKind::Edit ; "write")]
    #[test_case("edit", ToolKind::Edit ; "edit")]
    #[test_case("multiedit", ToolKind::Edit ; "multiedit")]
    #[test_case("apply_patch", ToolKind::Edit ; "apply_patch")]
    #[test_case("grep", ToolKind::Search ; "grep")]
    #[test_case("glob", ToolKind::Search ; "glob")]
    #[test_case("index", ToolKind::Search ; "index")]
    #[test_case("bash", ToolKind::Execute ; "bash")]
    #[test_case("code_execution", ToolKind::Execute ; "code_execution")]
    #[test_case("webfetch", ToolKind::Fetch ; "webfetch")]
    #[test_case("websearch", ToolKind::Fetch ; "websearch")]
    #[test_case("task", ToolKind::Think ; "task")]
    #[test_case("review", ToolKind::Think ; "review")]
    #[test_case("check", ToolKind::Think ; "check")]
    #[test_case("todowrite", ToolKind::Other ; "todowrite_is_other")]
    #[test_case("question", ToolKind::Other ; "unknown_is_other")]
    fn tool_kind_mapping(name: &str, expected: ToolKind) {
        assert_eq!(tool_kind(name), expected);
    }

    #[test]
    fn diff_output_renders_structured_diff() {
        let output = ToolOutput::Diff {
            path: "src/lib.rs".into(),
            before: "old\n".into(),
            after: "new\n".into(),
            summary: "+1 -1".into(),
        };
        let content = render_content(&output);
        assert_eq!(content.len(), 1);
        match &content[0] {
            ToolCallContent::Diff(d) => {
                assert_eq!(d.path, PathBuf::from("src/lib.rs"));
                assert_eq!(d.old_text.as_deref(), Some("old\n"));
                assert_eq!(d.new_text, "new\n");
            }
            other => panic!("expected diff content, got {other:?}"),
        }
    }

    #[test]
    fn todolist_emits_no_inline_content() {
        assert!(render_content(&ToolOutput::TodoList(Vec::new())).is_empty());
    }

    #[test]
    fn empty_plain_output_emits_no_content() {
        assert!(render_content(&ToolOutput::Plain(String::new())).is_empty());
    }

    #[test]
    fn read_code_renders_as_text_content() {
        let output = ToolOutput::ReadCode {
            path: "x.rs".into(),
            start_line: 1,
            lines: vec!["fn main() {}".into()],
            total_lines: 1,
            instructions: None,
            no_compress: false,
        };
        let content = render_content(&output);
        assert_eq!(content.len(), 1);
        assert!(matches!(content[0], ToolCallContent::Content(_)));
    }
}

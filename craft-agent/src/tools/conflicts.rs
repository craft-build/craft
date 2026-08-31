use craft_tool_macro::Tool;
use serde::Deserialize;

use argosy::codetools::conflicts::{self, ConflictsParams};

use crate::ToolOutput;

const CONFLICT_START: &str = "<<<<<<< ";
const CONFLICT_SEPARATOR: &str = "=======";
const CONFLICT_END: &str = ">>>>>>> ";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Conflicts {
    #[param(description = "Directory to scan (default: cwd)")]
    path: Option<String>,
    #[param(
        description = "Resolve conflicts instead of listing. Values: \"@theirs\" (incoming/their branch), \"@ours\" (current/our branch), \"@base\" (remove both sides). Omit to list."
    )]
    resolve: Option<String>,
    #[param(
        description = "Resolve only the Nth conflict (1-indexed) in each file. Omit to resolve all conflicts in scope."
    )]
    index: Option<usize>,
}

impl Conflicts {
    pub const NAME: &str = "conflicts";
    pub const DESCRIPTION: &str = include_str!("conflicts.md");
    pub const EXAMPLES: Option<&str> = None;

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let params = ConflictsParams {
            path: self.path.clone(),
            resolve: self.resolve.clone(),
            index: self.index,
        };
        let report = conflicts::run(&ctx.code_tools, params).map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(report.text))
    }

    pub fn start_header(&self) -> String {
        "conflicts".to_string()
    }
}

pub(super) struct ConflictMarker {
    pub start_line: usize,
    pub end_line: usize,
    pub our_branch: String,
    pub their_branch: String,
}

fn parse_conflicts(content: &str) -> Vec<ConflictMarker> {
    let mut markers = Vec::new();
    let mut current: Option<ConflictMarker> = None;

    for (i, line) in content.lines().enumerate() {
        if let Some(branch) = line.strip_prefix(CONFLICT_START) {
            current = Some(ConflictMarker {
                start_line: i + 1,
                end_line: 0,
                our_branch: branch.trim().to_string(),
                their_branch: String::new(),
            });
        } else if line == CONFLICT_SEPARATOR && current.is_some() {
        } else if let Some(branch) = line.strip_prefix(CONFLICT_END)
            && let Some(mut m) = current.take()
        {
            m.end_line = i + 1;
            m.their_branch = branch.trim().to_string();
            markers.push(m);
        }
    }

    markers
}

pub(super) fn collect_conflicts(scope_path: &str) -> Vec<(String, Vec<ConflictMarker>)> {
    let builder = ignore::WalkBuilder::new(scope_path)
        .hidden(true)
        .git_ignore(true)
        .build();

    let mut conflicts = Vec::new();
    for entry in builder.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let markers = parse_conflicts(&content);
        if !markers.is_empty() {
            let rel = super::relative_path(&path.to_string_lossy());
            conflicts.push((rel, markers));
        }
    }
    conflicts
}

super::impl_tool!(Conflicts, kind = "conflicts", tier = super::ToolTier::Core,);

impl super::ToolInvocation for Conflicts {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Conflicts::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Conflicts::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    #[test]
    fn parse_conflicts_finds_single() {
        let content = "\
some code
<<<<<<< HEAD
our change
=======
their change
>>>>>>> feature
more code";
        let markers = parse_conflicts(content);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].start_line, 2);
        assert_eq!(markers[0].end_line, 6);
        assert_eq!(markers[0].our_branch, "HEAD");
        assert_eq!(markers[0].their_branch, "feature");
    }

    #[test]
    fn parse_conflicts_finds_multiple() {
        let content = "\
<<<<<<< a
x
=======
y
>>>>>>> b
code
<<<<<<< c
p
=======
q
>>>>>>> d";
        let markers = parse_conflicts(content);
        assert_eq!(markers.len(), 2);
    }

    #[test]
    fn parse_conflicts_no_markers() {
        let content = "clean file\nno conflicts\n";
        let markers = parse_conflicts(content);
        assert!(markers.is_empty());
    }

    const CONFLICT_TEXT: &str = "\
top
<<<<<<< HEAD
ours-line
=======
theirs-line
>>>>>>> feature
bottom
<<<<<<< HEAD
second-ours
=======
second-theirs
>>>>>>> other
end";

    #[test_case("@theirs", "theirs" ; "json_theirs")]
    #[test_case("@ours", "ours" ; "json_ours")]
    #[test_case("@base", "base" ; "json_base")]
    fn parse_resolve_choice(choice: &str, _label: &str) {
        let tool: Conflicts = serde_json::from_value(json!({"resolve": choice})).unwrap();
        assert!(tool.resolve.is_some());
    }

    #[test_case("@theirs", "top\ntheirs-line\nbottom\nsecond-theirs\nend" ; "resolve_all_theirs")]
    #[test_case("@ours", "top\nours-line\nbottom\nsecond-ours\nend" ; "resolve_all_ours")]
    #[test_case("@base", "top\nbottom\nend" ; "resolve_all_base")]
    fn resolve_content_all(choice: &str, expected: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflicted.txt");
        std::fs::write(&path, CONFLICT_TEXT).unwrap();
        let tool: Conflicts = serde_json::from_value(json!({
            "path": path.to_string_lossy(),
            "resolve": choice,
        }))
        .unwrap();
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        futures::executor::block_on(tool.execute(&ctx)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn resolve_only_nth_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflicted.txt");
        std::fs::write(&path, CONFLICT_TEXT).unwrap();
        let tool: Conflicts = serde_json::from_value(json!({
            "path": path.to_string_lossy(),
            "resolve": "@theirs",
            "index": 2,
        }))
        .unwrap();
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        futures::executor::block_on(tool.execute(&ctx)).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(
            out.contains("ours-line"),
            "first conflict should be untouched"
        );
        assert!(out.contains("second-theirs"));
    }

    #[test]
    fn resolve_unknown_choice_errors() {
        let tool: Conflicts = serde_json::from_value(json!({"resolve": "@nope"})).unwrap();
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        let err = futures::executor::block_on(tool.execute(&ctx)).unwrap_err();
        assert!(err.contains("unknown resolve choice"));
    }

    #[test]
    fn list_reports_no_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("clean.txt"), "ok\n").unwrap();
        let tool: Conflicts = serde_json::from_value(json!({
            "path": dir.path().to_string_lossy(),
        }))
        .unwrap();
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        let out = futures::executor::block_on(tool.execute(&ctx)).unwrap();
        assert_eq!(out.as_text(), "no merge conflicts found");
    }
}

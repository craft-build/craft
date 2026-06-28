use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::relative_path;

const CONFLICT_START: &str = "<<<<<<< ";
const CONFLICT_SEPARATOR: &str = "=======";
const CONFLICT_END: &str = ">>>>>>> ";
const THEIRS: &str = "@theirs";
const OURS: &str = "@ours";
const BASE: &str = "@base";

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

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        if let Some(choice) = self.resolve.as_deref() {
            return self.resolve_conflicts(choice).await;
        }
        let scope = self.path.as_deref().unwrap_or(".").to_string();
        let scope_path = super::resolve_path(&scope).map_err(|e| format!("invalid path: {e}"))?;

        let conflicts = tokio::task::spawn_blocking(move || collect_conflicts(&scope_path))
            .await
            .map_err(|e| format!("conflicts scan failed: {e}"))?;

        if conflicts.is_empty() {
            return Ok(ToolOutput::Plain("no merge conflicts found".into()));
        }

        let mut out = format!("merge conflicts in {} file(s):\n", conflicts.len());
        for (file, markers) in &conflicts {
            out.push_str(&format!("\n{file} ({} conflict(s)):\n", markers.len()));
            for m in markers {
                out.push_str(&format!(
                    "  {} - {}: {} vs {}\n",
                    m.start_line, m.end_line, m.our_branch, m.their_branch
                ));
            }
        }
        Ok(ToolOutput::Plain(out))
    }

    async fn resolve_conflicts(&self, choice: &str) -> Result<ToolOutput, String> {
        let side = match choice {
            THEIRS => ConflictSide::Theirs,
            OURS => ConflictSide::Ours,
            BASE => ConflictSide::Base,
            other => {
                return Err(format!(
                    "unknown resolve choice \"{other}\"; use {THEIRS}, {OURS}, or {BASE}"
                ));
            }
        };
        let scope = self.path.as_deref().unwrap_or(".").to_string();
        let scope_path = super::resolve_path(&scope).map_err(|e| format!("invalid path: {e}"))?;
        let index = self.index;
        let (resolved_files, total_conflicts, remaining) =
            tokio::task::spawn_blocking(move || resolve_in_scope(&scope_path, side, index))
                .await
                .map_err(|e| format!("conflicts resolve failed: {e}"))?;

        if resolved_files.is_empty() {
            return Ok(ToolOutput::Plain(format!(
                "no conflicts resolved ({total_conflicts} found, {remaining} remaining)"
            )));
        }
        let mut out = format!(
            "resolved {total_conflicts} conflict(s) as {choice} in {} file(s):\n",
            resolved_files.len()
        );
        for f in &resolved_files {
            out.push_str(&format!("  {f}\n"));
        }
        if remaining > 0 {
            out.push_str(&format!("{remaining} conflict(s) remain unresolved\n"));
        }
        Ok(ToolOutput::Plain(out))
    }

    pub fn start_header(&self) -> String {
        "conflicts".to_string()
    }
}

#[derive(Debug, Clone)]
pub(super) struct ConflictMarker {
    pub(super) start_line: usize,
    pub(super) end_line: usize,
    pub(super) our_branch: String,
    pub(super) their_branch: String,
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

#[derive(Clone, Copy)]
enum ConflictSide {
    Ours,
    Theirs,
    Base,
}

/// Rewrite a file's content, resolving conflict markers according to `side`.
/// `index` selects only the Nth (1-indexed) conflict; `None` resolves all.
/// Returns the new content and the number of conflicts resolved.
fn resolve_content(content: &str, side: ConflictSide, index: Option<usize>) -> (String, usize) {
    let mut out = String::with_capacity(content.len());
    let mut state = 0u8;
    let mut ours = String::new();
    let mut theirs = String::new();
    let mut count = 0usize;
    let mut resolved = 0usize;
    let want = index.is_some();
    let want_n = index.unwrap_or(0);

    for line in content.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        match state {
            0 => {
                if bare.starts_with(CONFLICT_START) {
                    count += 1;
                    state = 1;
                    ours.clear();
                    theirs.clear();
                } else {
                    out.push_str(line);
                }
            }
            1 => {
                if bare == CONFLICT_SEPARATOR {
                    state = 2;
                } else {
                    ours.push_str(line);
                }
            }
            _ => {
                if let Some(_branch) = bare.strip_prefix(CONFLICT_END) {
                    let target = if want && want_n != count {
                        None
                    } else {
                        Some(match side {
                            ConflictSide::Ours => ours.as_str(),
                            ConflictSide::Theirs => theirs.as_str(),
                            ConflictSide::Base => "",
                        })
                    };
                    match target {
                        Some(t) => {
                            out.push_str(t);
                            resolved += 1;
                        }
                        None => {
                            out.push_str(CONFLICT_START);
                            out.push_str(" ours\n");
                            out.push_str(&ours);
                            out.push_str(CONFLICT_SEPARATOR);
                            out.push('\n');
                            out.push_str(&theirs);
                            out.push_str(CONFLICT_END);
                            out.push_str(" theirs\n");
                        }
                    }
                    state = 0;
                } else {
                    theirs.push_str(line);
                }
            }
        }
    }
    let _ = want;
    (out, resolved)
}

fn resolve_in_scope(
    scope_path: &str,
    side: ConflictSide,
    index: Option<usize>,
) -> (Vec<String>, usize, usize) {
    let files = collect_conflict_files(scope_path);
    let mut resolved_files = Vec::new();
    let mut total_resolved = 0usize;
    let mut remaining = 0usize;

    for (path, content) in files {
        let (new_content, resolved) = resolve_content(&content, side, index);
        let marker_count = parse_conflicts(&content).len();
        remaining += marker_count.saturating_sub(resolved);
        if resolved > 0 {
            let _ = std::fs::write(&path, &new_content);
            total_resolved += resolved;
            resolved_files.push(relative_path(&path));
        }
    }
    (resolved_files, total_resolved, remaining)
}

fn collect_conflict_files(scope_path: &str) -> Vec<(String, String)> {
    let builder = ignore::WalkBuilder::new(scope_path)
        .hidden(true)
        .git_ignore(true)
        .build();
    let mut out = Vec::new();
    for entry in builder.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Ok(content) = std::fs::read_to_string(path)
            && content.contains(CONFLICT_START)
        {
            out.push((path.to_string_lossy().into_owned(), content));
        }
    }
    out
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
            let rel = relative_path(&path.to_string_lossy());
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

    #[test_case(ConflictSide::Ours, "top\nours-line\nbottom\nsecond-ours\nend" ; "resolve_all_ours")]
    #[test_case(ConflictSide::Theirs, "top\ntheirs-line\nbottom\nsecond-theirs\nend" ; "resolve_all_theirs")]
    #[test_case(ConflictSide::Base, "top\nbottom\nend" ; "resolve_all_base")]
    fn resolve_content_all(side: ConflictSide, expected: &str) {
        let (out, count) = resolve_content(CONFLICT_TEXT, side, None);
        assert_eq!(count, 2);
        assert_eq!(out, expected);
    }

    #[test]
    fn resolve_content_only_nth_keeps_others() {
        let (out, count) = resolve_content(CONFLICT_TEXT, ConflictSide::Theirs, Some(2));
        assert_eq!(count, 1);
        assert!(
            out.contains("ours-line"),
            "first conflict should be untouched"
        );
        assert!(out.contains("second-theirs"));
    }

    #[test_case(json!({"resolve": "@theirs"}), "theirs" ; "json_theirs")]
    #[test_case(json!({"resolve": "@ours"}), "ours" ; "json_ours")]
    #[test_case(json!({"resolve": "@base"}), "base" ; "json_base")]
    fn parse_resolve_choice(input: serde_json::Value, _label: &str) {
        let tool: Conflicts = serde_json::from_value(input).unwrap();
        assert!(tool.resolve.is_some());
    }

    #[test]
    fn resolve_unknown_choice_errors() {
        let tool = Conflicts {
            path: None,
            resolve: Some("@nope".into()),
            index: None,
        };
        let err = futures::executor::block_on(tool.resolve_conflicts("@nope")).unwrap_err();
        assert!(err.contains("unknown resolve choice"));
    }
}

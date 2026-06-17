use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::relative_path;

const CONFLICT_START: &str = "<<<<<<< ";
const CONFLICT_SEPARATOR: &str = "=======";
const CONFLICT_END: &str = ">>>>>>> ";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Conflicts {
    #[param(description = "Directory to scan (default: cwd)")]
    path: Option<String>,
}

impl Conflicts {
    pub const NAME: &str = "conflicts";
    pub const DESCRIPTION: &str = include_str!("conflicts.md");
    pub const EXAMPLES: Option<&str> = None;

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
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

    pub fn start_header(&self) -> String {
        "conflicts".to_string()
    }
}

#[derive(Debug)]
struct ConflictMarker {
    start_line: usize,
    end_line: usize,
    our_branch: String,
    their_branch: String,
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

fn collect_conflicts(scope_path: &str) -> Vec<(String, Vec<ConflictMarker>)> {
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
}

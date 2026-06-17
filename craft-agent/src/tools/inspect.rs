use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Inspect {
    #[param(description = "Sections: todos, git_status, or all (default all)")]
    sections: Option<String>,
    #[param(description = "File or directory to scope (default: cwd)")]
    scope: Option<String>,
}

impl Inspect {
    pub const NAME: &str = "inspect";
    pub const DESCRIPTION: &str = include_str!("inspect.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"sections": "all"}, {"sections": "todos", "scope": "src/lib.rs"}]"#);

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let sections = self.sections.as_deref().unwrap_or("all").to_string();
        let scope = self.scope.as_deref().unwrap_or(".").to_string();

        let (todos_out, git_out) = tokio::task::spawn_blocking(
            move || -> Result<(Option<String>, Option<String>), String> {
                let todos = if sections == "all" || sections == "todos" {
                    Some(inspect_todos(&scope)?)
                } else {
                    None
                };
                let git = if sections == "all" || sections == "git_status" {
                    Some(inspect_git_status(&scope)?)
                } else {
                    None
                };
                Ok((todos, git))
            },
        )
        .await
        .map_err(|e| format!("inspect failed: {e}"))??;

        let mut out = String::new();
        if let Some(t) = todos_out {
            out.push_str(&t);
        }
        if let Some(g) = git_out {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&g);
        }

        if out.is_empty() {
            out.push_str("nothing to inspect");
        }

        Ok(ToolOutput::Plain(out))
    }

    pub fn start_header(&self) -> String {
        format!("inspect {}", self.sections.as_deref().unwrap_or("all"))
    }
}

fn inspect_todos(scope: &str) -> Result<String, String> {
    let scope_path = super::resolve_path(scope).map_err(|e| format!("invalid scope: {e}"))?;
    let path = std::path::Path::new(&scope_path);

    let mut todos = Vec::new();

    if path.is_file() {
        collect_todos_from_file(path, &mut todos);
    } else {
        let builder = ignore::WalkBuilder::new(path)
            .hidden(true)
            .git_ignore(true)
            .build();
        for entry in builder.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            collect_todos_from_file(entry.path(), &mut todos);
        }
    }

    if todos.is_empty() {
        return Ok("todos: (none)\n".to_string());
    }

    let mut out = format!("todos: ({} items)\n", todos.len());
    for (file, line_no, text) in &todos {
        let rel = relative_path(file);
        let preview = truncate_todo(text, 80);
        out.push_str(&format!("  {rel}:{line_no}: {preview}\n"));
    }
    Ok(out)
}

fn collect_todos_from_file(path: &std::path::Path, todos: &mut Vec<(String, usize, String)>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let keywords = ["TODO", "FIXME", "HACK", "XXX"];
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        for kw in &keywords {
            if let Some(rest) = find_keyword(trimmed, kw) {
                let text = rest
                    .strip_prefix(':')
                    .or_else(|| rest.strip_prefix('('))
                    .or_else(|| rest.strip_prefix(' '))
                    .unwrap_or(rest)
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    todos.push((path.to_string_lossy().to_string(), i + 1, text));
                }
                break;
            }
        }
    }
}

fn find_keyword<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    if let Some(rest) = line.strip_prefix(keyword) {
        return Some(rest);
    }
    for comment_prefix in &["// ", "# ", "-- ", ";; ", "/* ", "<!-- "] {
        if let Some(after_comment) = line.strip_prefix(comment_prefix)
            && let Some(rest) = after_comment.strip_prefix(keyword)
        {
            return Some(rest);
        }
    }
    None
}

fn truncate_todo(text: &str, max: usize) -> String {
    if text.len() > max {
        let boundary = text.floor_char_boundary(max.saturating_sub(3));
        format!("{}...", &text[..boundary])
    } else {
        text.to_string()
    }
}

fn inspect_git_status(scope: &str) -> Result<String, String> {
    let scope_path = super::resolve_path(scope).map_err(|e| format!("invalid scope: {e}"))?;
    let path = std::path::Path::new(&scope_path);

    let repo_dir = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    let output = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "--"])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;

    if !output.status.success() {
        return Ok("git_status: (not a git repo)\n".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.is_empty() {
        return Ok("git_status: (clean)\n".to_string());
    }

    let entries: Vec<&str> = stdout.lines().take(50).collect();
    let total = stdout.lines().count();

    let mut out = format!("git_status: ({} changes)\n", total);
    for entry in &entries {
        out.push_str(&format!("  {entry}\n"));
    }
    if total > entries.len() {
        out.push_str(&format!("  ... ({} more)\n", total - entries.len()));
    }
    Ok(out)
}

super::impl_tool!(Inspect, kind = "inspect", tier = super::ToolTier::Core,);

impl super::ToolInvocation for Inspect {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Inspect::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Inspect::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_todos_from_file_finds_todo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "fn main() {\n  // TODO: fix this\n}\n").unwrap();
        let mut todos = Vec::new();
        collect_todos_from_file(&path, &mut todos);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].2, "fix this");
    }

    #[test]
    fn collect_todos_from_file_finds_fixme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.py");
        std::fs::write(&path, "# FIXME: broken\npass\n").unwrap();
        let mut todos = Vec::new();
        collect_todos_from_file(&path, &mut todos);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].2, "broken");
    }

    #[test]
    fn truncate_todo_short() {
        assert_eq!(truncate_todo("hello", 80), "hello");
    }

    #[test]
    fn truncate_todo_long() {
        let long = "x".repeat(100);
        let result = truncate_todo(&long, 80);
        assert!(result.len() <= 80);
        assert!(result.ends_with("..."));
    }
}

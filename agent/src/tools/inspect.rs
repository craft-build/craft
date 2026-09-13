use std::{fs, path::Path, process::Command};

use ignore::WalkBuilder;
use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{MAX_FILE_BYTES, Result, Workspace, impl_tool, invalid};

const TODO_KEYWORDS: [&str; 4] = ["TODO", "FIXME", "HACK", "XXX"];
const COMMENT_PREFIXES: [&str; 6] = ["// ", "# ", "-- ", ";; ", "/* ", "<!-- "];
const PREVIEW_MAX_BYTES: usize = 80;
const GIT_STATUS_MAX_ENTRIES: usize = 50;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectArgs {
    /// Sections to run: `todos`, `git_status`, or `all` (default `all`).
    pub sections: Option<String>,
    /// File or directory to scope, relative to the workspace (default `.`).
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    ".".into()
}

#[derive(Debug)]
pub struct InspectOutput {
    pub text: String,
}

impl IntoToolOutput for InspectOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(self.text))
    }
}

#[derive(Clone)]
pub struct Inspect(pub Workspace);

impl Inspect {
    fn execute(workspace: &Workspace, args: InspectArgs) -> Result<InspectOutput> {
        let sections = args.sections.as_deref().unwrap_or("all");
        if !matches!(sections, "todos" | "git_status" | "all") {
            return Err(invalid("sections must be one of: todos, git_status, all"));
        }
        let scope = workspace.resolve(&args.scope)?;
        let mut out = String::new();
        if matches!(sections, "all" | "todos") {
            out.push_str(&inspect_todos(workspace, &scope));
        }
        if matches!(sections, "all" | "git_status") {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&inspect_git_status(&scope)?);
        }
        if out.is_empty() {
            out.push_str("nothing to inspect");
        }
        Ok(InspectOutput { text: out })
    }
}

fn inspect_todos(workspace: &Workspace, scope: &Path) -> String {
    let mut todos = Vec::new();
    if scope.is_file() {
        collect_todos_from_file(workspace, scope, &mut todos);
    } else {
        let walk = WalkBuilder::new(scope)
            .hidden(true)
            .git_ignore(true)
            .build();
        for entry in walk.flatten() {
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                collect_todos_from_file(workspace, entry.path(), &mut todos);
            }
        }
    }
    if todos.is_empty() {
        return "todos: (none)\n".into();
    }
    let mut out = format!("todos: ({} items)\n", todos.len());
    for (file, line, text) in &todos {
        out.push_str(&format!("  {file}:{line}: {}\n", truncate_todo(text)));
    }
    out
}

fn collect_todos_from_file(
    workspace: &Workspace,
    path: &Path,
    todos: &mut Vec<(String, usize, String)>,
) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        for keyword in TODO_KEYWORDS {
            if let Some(rest) = find_keyword(trimmed, keyword) {
                let text = rest
                    .strip_prefix(':')
                    .or_else(|| rest.strip_prefix('('))
                    .or_else(|| rest.strip_prefix(' '))
                    .unwrap_or(rest)
                    .trim()
                    .to_string();
                if !text.is_empty() {
                    todos.push((workspace.display(path), index + 1, text));
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
    for prefix in COMMENT_PREFIXES {
        if let Some(after) = line.strip_prefix(prefix)
            && let Some(rest) = after.strip_prefix(keyword)
        {
            return Some(rest);
        }
    }
    None
}

fn truncate_todo(text: &str) -> String {
    if text.len() <= PREVIEW_MAX_BYTES {
        return text.into();
    }
    let mut end = PREVIEW_MAX_BYTES.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn inspect_git_status(scope: &Path) -> Result<String> {
    let repo_dir = if scope.is_file() {
        scope.parent().unwrap_or(scope)
    } else {
        scope
    };
    let toplevel = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(repo_dir)
        .output()
        .map_err(|error| super::failure(format!("git status failed: {error}")))?;
    if !toplevel.status.success() {
        return Ok("git_status: (not a git repo)\n".into());
    }
    let root = String::from_utf8_lossy(&toplevel.stdout).trim().to_string();
    // Canonicalize so the strip works when the workspace was reached through
    // a symlinked path (e.g. /tmp on macOS).
    let canonical = fs::canonicalize(scope).unwrap_or_else(|_| scope.to_path_buf());
    // Repo-root-relative pathspec: git runs from the repo root, which may be
    // an ancestor of the workspace, so workspace-relative display is wrong.
    let rel = canonical
        .strip_prefix(&root)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut command = Command::new("git");
    command
        .args(["status", "--porcelain=v1"])
        .current_dir(&root);
    if !rel.is_empty() {
        command.arg("--").arg(&rel);
    }
    let output = command
        .output()
        .map_err(|error| super::failure(format!("git status failed: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(format!("git_status: (git failed: {stderr})\n"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.is_empty() {
        return Ok("git_status: (clean)\n".into());
    }
    let lines: Vec<&str> = stdout.lines().collect();
    let total = lines.len();
    let mut out = format!("git_status: ({total} changes)\n");
    for entry in lines.iter().take(GIT_STATUS_MAX_ENTRIES) {
        out.push_str(&format!("  {entry}\n"));
    }
    if total > GIT_STATUS_MAX_ENTRIES {
        out.push_str(&format!(
            "  ... ({} more)\n",
            total - GIT_STATUS_MAX_ENTRIES
        ));
    }
    Ok(out)
}

impl_tool!(
    Inspect,
    InspectArgs,
    InspectOutput,
    "inspect",
    "Quick project health check. Scans for TODO/FIXME/HACK/XXX comments behind common comment prefixes and reports `git status --porcelain` (scoped to the given file or directory, capped at 50 entries). Sections: todos, git_status, or all."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_keywords_behind_comment_prefixes() {
        for (line, expected) in [
            ("// TODO: fix", "fix"),
            ("# FIXME broken", "broken"),
            ("-- HACK(x): sneaky", "x): sneaky"),
            ("TODO plain", "plain"),
            ("nothing here", ""),
        ] {
            let found = TODO_KEYWORDS
                .iter()
                .find_map(|kw| find_keyword(line.trim(), kw))
                .map(|rest| {
                    rest.strip_prefix(':')
                        .or_else(|| rest.strip_prefix('('))
                        .or_else(|| rest.strip_prefix(' '))
                        .unwrap_or(rest)
                        .trim()
                        .to_string()
                })
                .unwrap_or_default();
            assert_eq!(found, expected, "line: {line}");
        }
    }

    #[test]
    fn truncate_todo_is_char_boundary_safe() {
        assert_eq!(truncate_todo("short"), "short");
        let long = format!("{}β tail", "x".repeat(100));
        let truncated = truncate_todo(&long);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= PREVIEW_MAX_BYTES);
    }
}

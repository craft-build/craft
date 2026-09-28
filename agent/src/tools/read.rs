use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{
    MAX_LINE_BYTES, MAX_OUTPUT_BYTES, Result, Workspace, clip, impl_tool, invalid, read_bytes, text,
};
use crate::skills::Discovery;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Workspace-relative path, or an absolute path inside the workspace.
    pub path: String,
    /// First line, one-based.
    #[serde(default = "default_offset")]
    pub offset: usize,
    /// Lines to return (default 200, maximum 2000; 0 means 2000).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_offset() -> usize {
    1
}
fn default_limit() -> usize {
    200
}

#[derive(Debug)]
pub struct ReadLine {
    pub number: usize,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ReadOutput {
    pub path: String,
    pub lines: Vec<ReadLine>,
    pub total_lines: usize,
    pub next_offset: Option<usize>,
    /// Subdirectory instruction files discovered for the read file's parent
    /// directory, as `(canonical_path, content)` pairs.
    pub instructions: Vec<(String, String)>,
}

impl IntoToolOutput for ReadOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut text = self
            .lines
            .iter()
            .map(|line| {
                format!(
                    "{}: {}{}",
                    line.number,
                    line.text,
                    if line.truncated { "..." } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(offset) = self.next_offset {
            text.push_str(&format!(
                "\n\n...\n\nTruncated lines: {offset}-{}. Use offset={offset} to read further.",
                self.total_lines
            ));
        }
        for (path, content) in &self.instructions {
            text.push_str(&format!("\n\n---\nInstructions from: {path}\n{content}"));
        }
        Ok(ToolOutput::text(text))
    }
}

#[derive(Clone)]
pub struct Read(pub Workspace);

impl Read {
    fn execute(workspace: &Workspace, args: ReadArgs) -> Result<ReadOutput> {
        if args.offset == 0 || args.limit > 2000 {
            return Err(invalid(
                "offset must be >= 1 and limit must be between 0 and 2000",
            ));
        }
        if let Some(result) = read_skill(&args.path, &skill_discovery(workspace), workspace) {
            return result;
        }
        let path = workspace.file(&args.path)?;
        let contents = text(read_bytes(&path)?)?;
        let total_lines = contents.lines().count();
        if args.offset > total_lines.saturating_add(1) {
            return Err(invalid(format!(
                "offset exceeds end of file ({total_lines} lines)"
            )));
        }
        let limit = if args.limit == 0 { 2000 } else { args.limit };
        let mut lines = Vec::new();
        let mut bytes = 0;
        for (index, line) in contents
            .lines()
            .enumerate()
            .skip(args.offset - 1)
            .take(limit)
        {
            let (line, truncated) = clip(line, MAX_LINE_BYTES);
            if bytes + line.len() > MAX_OUTPUT_BYTES {
                break;
            }
            bytes += line.len();
            lines.push(ReadLine {
                number: index + 1,
                text: line.into(),
                truncated,
            });
        }
        let next = args.offset + lines.len();
        let instructions = if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(crate::instructions::is_instruction_file)
        {
            Vec::new()
        } else {
            let parent = path.parent().unwrap_or(&path);
            crate::instructions::find_subdirectory_instructions(
                parent,
                workspace.root(),
                &workspace.loaded_instructions,
            )
        };
        Ok(ReadOutput {
            path: workspace.display(&path),
            lines,
            total_lines,
            next_offset: (next <= total_lines).then_some(next),
            instructions,
        })
    }
}

/// `skill://<name>` internal URL scheme: resolve a discovered skill and
/// return its full SKILL.md body with a location header. Returns `None`
/// when the path is not a skill URL, so ordinary reads proceed.
fn read_skill(
    path: &str,
    discovery: &Discovery,
    workspace: &Workspace,
) -> Option<Result<ReadOutput>> {
    let name = path.strip_prefix("skill://")?;
    let name = name.trim();
    if name.is_empty() {
        return Some(Err(invalid("skill:// requires a skill name")));
    }
    let Some(skill) = discovery.find(name) else {
        return Some(Err(invalid(format!(
            "skill '{name}' not found{}",
            discovery.skill_list()
        ))));
    };
    let is_builtin = skill.scope.is_builtin();
    let location = if is_builtin {
        skill.location()
    } else {
        workspace.display(&skill.path)
    };
    let body = if is_builtin {
        skill.content
    } else {
        let not_symlink = std::fs::symlink_metadata(&skill.path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        if !not_symlink {
            return Some(Err(invalid(format!(
                "skill '{name}' marker is not a regular file"
            ))));
        }
        match read_bytes(&skill.path).and_then(text) {
            Ok(body) => body,
            Err(error) => {
                return Some(Err(invalid(format!(
                    "failed to read skill '{name}': {error}"
                ))));
            }
        }
    };

    let mut lines = Vec::new();
    let mut bytes = 0;
    for (index, line) in body.lines().enumerate() {
        let (line, truncated) = clip(line, MAX_LINE_BYTES);
        if bytes + line.len() > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += line.len();
        lines.push(ReadLine {
            number: index + 1,
            text: line.into(),
            truncated,
        });
    }
    let total_lines = body.lines().count();
    Some(Ok(ReadOutput {
        path: format!("skill://{name} ({location})"),
        lines,
        total_lines,
        next_offset: None,
        instructions: Vec::new(),
    }))
}

/// Skill discovery rooted at the workspace, so `skill://` reads follow the
/// project the session is working in.
fn skill_discovery(workspace: &Workspace) -> Discovery {
    Discovery::new(
        workspace.root().to_path_buf(),
        crate::paths::home(),
        crate::paths::xdg_config_dir().ok(),
    )
}

impl_tool!(
    Read,
    ReadArgs,
    ReadOutput,
    "read",
    "Read UTF-8 text with one-based line numbers. Use offset/limit for paging and the returned truncation hint to continue. Files are capped at 8 MiB; lines longer than 2048 bytes end with '...'. No binary files, symlinks, or paths outside the workspace. Also accepts the internal URL scheme 'skill://<name>' to read a discovered skill body (SKILL.md)."
);

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace() -> (TempDir, Workspace) {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let ws = Workspace::new(tmp.path()).unwrap();
        (tmp, ws)
    }

    fn discovery(root: &std::path::Path) -> Discovery {
        // Canonicalize to match `Workspace::new`, so `display` strips the root.
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Discovery::new(root, None, None)
    }

    #[test]
    fn skill_url_reads_builtin_body() {
        let (tmp, ws) = workspace();
        let out = read_skill("skill://run", &discovery(tmp.path()), &ws)
            .expect("skill url handled")
            .expect("builtin found");
        assert_eq!(out.path, "skill://run (<builtin>/skills/run/SKILL.md)");
        assert_eq!(
            out.total_lines,
            crate::skills::builtin("run").unwrap().lines().count()
        );
        assert_eq!(out.lines[0].text, "---");
        assert_eq!(out.next_offset, None);
    }

    #[test]
    fn skill_url_reads_project_skill_relative_location() {
        let (tmp, ws) = workspace();
        let path = tmp.path().join(".craft/skills/audit/SKILL.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nname: audit\n---\naudit body").unwrap();
        let out = read_skill("skill://audit", &discovery(tmp.path()), &ws)
            .expect("skill url handled")
            .expect("project skill found");
        assert_eq!(out.path, "skill://audit (.craft/skills/audit/SKILL.md)");
        assert!(out.lines.iter().any(|l| l.text.contains("audit body")));
    }

    #[test]
    fn unknown_skill_errors_with_list() {
        let (tmp, ws) = workspace();
        let error = read_skill("skill://nope", &discovery(tmp.path()), &ws)
            .expect("skill url handled")
            .expect_err("not found");
        let message = error.to_string();
        assert!(message.contains("skill 'nope' not found"), "{message}");
        assert!(message.contains("- verify:"), "{message}");
    }

    #[test]
    fn empty_skill_selector_errors() {
        let (tmp, ws) = workspace();
        assert!(
            read_skill("skill://", &discovery(tmp.path()), &ws)
                .expect("handled")
                .is_err()
        );
    }

    #[test]
    fn non_skill_paths_fall_through() {
        let (tmp, ws) = workspace();
        assert!(read_skill("src/main.rs", &discovery(tmp.path()), &ws).is_none());
        assert!(read_skill("http://example.com", &discovery(tmp.path()), &ws).is_none());
    }

    #[test]
    fn builtin_read_ignores_paging() {
        let (tmp, ws) = workspace();
        let out = read_skill("skill://verify", &discovery(tmp.path()), &ws)
            .expect("handled")
            .expect("found");
        assert_eq!(out.lines.len(), out.total_lines);
    }
}

//! Move/rename a file (or directory) inside the workspace, then update
//! project-wide imports that referenced the old module path.
//!
//! Ports the reference `craft-agent/src/tools/move_file.rs` onto this repo's
//! workspace model: paths flow through the shared validation walk, the rename
//! runs under the workspace lock on the blocking worker, and every rewritten
//! file is snapshotted for `/undo` before it is touched.

use std::path::{Path, PathBuf};

use regex::Regex;
use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, Workspace, impl_tool, invalid, io_error, read_bytes, text};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveFileArgs {
    /// Source file or directory path inside the workspace.
    pub source: String,
    /// Destination path inside the workspace. Missing parent directories are
    /// created; an existing destination must be a regular file.
    pub destination: String,
}

#[derive(Debug)]
pub struct MoveFileOutput {
    pub source: String,
    pub destination: String,
    pub import_updates: Vec<(String, usize)>,
}

impl IntoToolOutput for MoveFileOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        let mut out = format!("moved {} -> {}", self.source, self.destination);
        if !self.import_updates.is_empty() {
            out.push_str(&format!(
                "\nupdated imports in {} file(s)",
                self.import_updates.len()
            ));
            for (file, count) in &self.import_updates {
                out.push_str(&format!("\n  {file}: {count} reference(s)"));
            }
        }
        Ok(ToolOutput::text(out))
    }
}

#[derive(Clone)]
pub struct MoveFile(pub Workspace);

impl MoveFile {
    fn execute(workspace: &Workspace, args: MoveFileArgs) -> Result<MoveFileOutput> {
        let src = workspace.resolve(&args.source)?;
        let dst = workspace.target(&args.destination)?;

        let src_is_file = fs_metadata(&src).map_err(io_error)?.is_file();
        if src_is_file {
            workspace.note_snapshot(&src);
        }
        // `rename` silently overwrites an existing destination file; snapshot
        // it too so the overwrite is reversible.
        if fs_metadata(&dst).map(|m| m.is_file()).unwrap_or(false) {
            workspace.note_snapshot(&dst);
        }

        std::fs::rename(&src, &dst).map_err(io_error)?;

        let import_updates = update_imports(workspace, &src, &dst)?;
        Ok(MoveFileOutput {
            source: workspace.display(&src),
            destination: workspace.display(&dst),
            import_updates,
        })
    }
}

fn fs_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

/// Paths listed in a `move` result's import-update section. The read
/// lifecycle uses this to mark prior reads of rewritten files stale.
pub(crate) fn rewritten_files(output: &str) -> Vec<String> {
    output
        .lines()
        .skip_while(|line| !line.starts_with("updated imports in"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.trim_start().split(": ").next())
        .map(String::from)
        .collect()
}

/// Rewrite imports across the workspace after a move. Only files whose module
/// path actually changed are scanned; every rewrite is snapshotted first.
fn update_imports(
    workspace: &Workspace,
    old_path: &Path,
    new_path: &Path,
) -> Result<Vec<(String, usize)>> {
    let old_module_path = file_path_to_module_path(&workspace.display(old_path));
    let new_module_path = file_path_to_module_path(&workspace.display(new_path));
    if old_module_path == new_module_path {
        return Ok(Vec::new());
    }

    let re = module_path_regex(&old_module_path).map_err(invalid)?;
    let mut updates = Vec::new();
    for path in source_files(workspace.root()) {
        let content = match read_bytes(&path).and_then(text) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let (count, new_content) = rewrite_imports(&content, &re, &new_module_path, ext);
        if count > 0 && new_content != content {
            workspace.note_snapshot(&path);
            std::fs::write(&path, &new_content).map_err(|e| {
                super::failure(format!("import rewrite failed for {}: {e}", path.display()))
            })?;
            updates.push((workspace.display(&path), count));
        }
    }
    Ok(updates)
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_some_and(|ft| ft.is_file())
                && entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(is_source_file)
        })
        .map(|entry| entry.into_path())
        .collect()
}

fn module_path_regex(old_module_path: &str) -> std::result::Result<Regex, String> {
    Regex::new(&format!(
        "(^|[^A-Za-z0-9_:]){}(::|[^A-Za-z0-9_]|$)",
        regex::escape(old_module_path)
    ))
    .map_err(|e| format!("invalid module path for regex: {e}"))
}

fn rewrite_imports(content: &str, re: &Regex, new_module_path: &str, ext: &str) -> (usize, String) {
    let mut count = 0;
    let mut out = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        if is_import_line(line, ext) {
            count += re.find_iter(line).count();
            out.push_str(&re.replace_all(line, |c: &regex::Captures| {
                format!("{}{}{}", &c[1], new_module_path, &c[2])
            }));
        } else {
            out.push_str(line);
        }
    }
    (count, out)
}

fn is_import_line(line: &str, ext: &str) -> bool {
    let t = line.trim_start();
    match ext {
        "rs" => {
            t.starts_with("use ") || t.starts_with("pub use ") || t.starts_with("extern crate ")
        }
        "ts" | "tsx" | "js" | "jsx" => t.starts_with("import ") || t.starts_with("export "),
        "py" => t.starts_with("import ") || t.starts_with("from "),
        "go" | "java" | "kt" => t.starts_with("import "),
        _ => false,
    }
}

fn file_path_to_module_path(path: &str) -> String {
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = path.strip_prefix("src/").unwrap_or(path);
    let path = path
        .strip_suffix("/mod.rs")
        .or_else(|| path.strip_suffix("/lib.rs"))
        .or_else(|| path.strip_suffix("/index.rs"))
        .or_else(|| path.strip_suffix("/index.ts"))
        .unwrap_or(path);

    if path.ends_with(".rs") || path.ends_with(".ts") || path.ends_with(".tsx") {
        &path[..path.rfind('.').unwrap_or(path.len())]
    } else {
        path
    }
    .replace(['/', '\\'], "::")
}

fn is_source_file(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "kt"
    )
}

impl_tool!(
    MoveFile,
    MoveFileArgs,
    MoveFileOutput,
    "move",
    "Move or rename a file or directory inside the workspace, updating project-wide import references (Rust use, TS/JS import/export, Python import/from, Go/Java/Kotlin import). Missing destination parent directories are created. Source and destination must stay inside the workspace; symlinks and Git metadata are refused."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_path_to_module_path_strips_src_and_ext() {
        assert_eq!(file_path_to_module_path("src/foo/bar.rs"), "foo::bar");
    }

    #[test]
    fn file_path_to_module_path_handles_mod_rs() {
        assert_eq!(file_path_to_module_path("src/foo/mod.rs"), "foo");
    }

    #[test]
    fn file_path_to_module_path_handles_index_ts() {
        assert_eq!(file_path_to_module_path("src/utils/index.ts"), "utils");
    }

    #[test]
    fn rewritten_files_parses_result_section() {
        assert_eq!(
            rewritten_files(
                "moved a.rs -> b.rs\nupdated imports in 2 file(s)\n  src/main.rs: 1 reference(s)\n  lib/x.ts: 2 reference(s)"
            ),
            vec!["src/main.rs".to_string(), "lib/x.ts".to_string()]
        );
        assert!(rewritten_files("moved a.rs -> b.rs").is_empty());
    }

    #[test]
    fn rewrite_imports_only_touches_import_lines() {
        let re = module_path_regex("foo::bar").unwrap();
        let content = "use foo::bar::Baz;\nconst COMMENT = \"foo::bar in a string\";\nlet x = foo::bar_value;\n";
        let (count, out) = rewrite_imports(content, &re, "foo::qux", "rs");
        assert_eq!(count, 1);
        assert!(out.contains("use foo::qux::Baz;"));
        assert!(out.contains("\"foo::bar in a string\""));
        assert!(out.contains("foo::bar_value"));
    }

    #[test]
    fn rewrite_imports_avoids_partial_segment_match() {
        let re = module_path_regex("bar").unwrap();
        let content = "use foo::bar::Baz;\nuse bar::Thing;\nuse foobar::Other;\n";
        let (count, out) = rewrite_imports(content, &re, "renamed", "rs");
        assert_eq!(count, 1);
        assert!(out.contains("use renamed::Thing;"));
        assert!(out.contains("use foo::bar::Baz;"));
        assert!(out.contains("use foobar::Other;"));
    }
}

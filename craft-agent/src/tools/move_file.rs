use std::path::{Path, PathBuf};

use craft_tool_macro::Tool;
use regex::Regex;
use serde::Deserialize;

use crate::ToolOutput;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct MoveFile {
    #[param(description = "Source file or directory path")]
    source: String,
    #[param(description = "Destination path")]
    destination: String,
}

impl MoveFile {
    pub const NAME: &str = "move";
    pub const DESCRIPTION: &str = include_str!("move_file.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"source": "src/old.rs", "destination": "src/new.rs"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let src = super::resolve_path(&self.source).map_err(|e| format!("invalid source: {e}"))?;
        let dst = super::resolve_path(&self.destination)
            .map_err(|e| format!("invalid destination: {e}"))?;

        let src_path = PathBuf::from(&src);
        let dst_path = PathBuf::from(&dst);

        if !src_path.exists() {
            return Err(format!("source not found: {}", relative_path(&src)));
        }

        ctx.file_tracker.check_before_edit(&src_path)?;

        if src_path.is_file()
            && let Ok(content) = ctx.fs.read_text_file(&src_path).await
        {
            ctx.snapshot_store.push_backup(src_path.clone(), content);
        }

        if let Some(parent) = dst_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create parent dir: {e}"))?;
        }

        let src_clone = src_path.clone();
        let dst_clone = dst_path.clone();
        tokio::task::spawn_blocking(move || std::fs::rename(&src_clone, &dst_clone))
            .await
            .map_err(|e| format!("rename task failed: {e}"))?
            .map_err(|e| format!("rename failed: {e}"))?;

        let src_rel = relative_path(&src);
        let dst_rel = relative_path(&dst);
        let mut out = format!("moved {src_rel} -> {dst_rel}");

        let import_updates = update_imports(ctx, &src, &dst).await?;
        if !import_updates.is_empty() {
            out.push_str(&format!(
                "\nupdated imports in {} file(s)",
                import_updates.len()
            ));
            for (file, count) in &import_updates {
                let rel = relative_path(file);
                out.push_str(&format!("\n  {rel}: {count} reference(s)"));
            }
        }

        Ok(ToolOutput::Plain(out))
    }

    pub fn start_header(&self) -> String {
        format!("move {} -> {}", self.source, self.destination)
    }
}

async fn update_imports(
    ctx: &super::ToolContext,
    old_path: &str,
    new_path: &str,
) -> Result<Vec<(String, usize)>, String> {
    let old_module_path = file_path_to_module_path(old_path);
    let new_module_path = file_path_to_module_path(new_path);

    if old_module_path == new_module_path {
        return Ok(Vec::new());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let candidates = tokio::task::spawn_blocking(move || {
        let builder = ignore::WalkBuilder::new(&cwd)
            .hidden(true)
            .git_ignore(true)
            .build();
        let mut out = Vec::new();
        for entry in builder.flatten() {
            if entry.file_type().is_some_and(|ft| ft.is_file())
                && entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(is_source_file)
            {
                out.push(entry.path().to_path_buf());
            }
        }
        out
    })
    .await
    .map_err(|e| format!("import scan failed: {e}"))?;

    let re = module_path_regex(&old_module_path)?;
    let mut updates = Vec::new();

    for path in candidates {
        ctx.file_tracker.check_before_edit(&path)?;
        let content = match ctx.fs.read_text_file(&path).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };

        let (count, new_content) = rewrite_imports(&content, &re, &new_module_path, ext);
        if count > 0 && new_content != content {
            ctx.snapshot_store.push_backup(path.clone(), content);
            ctx.fs
                .write_text_file(&path, &new_content)
                .await
                .map_err(|e| format!("import rewrite write failed for {}: {e}", path.display()))?;
            ctx.file_tracker.record_read(&path);
            updates.push((relative_path(&path.to_string_lossy()), count));
        }
    }

    Ok(updates)
}

fn module_path_regex(old_module_path: &str) -> Result<Regex, String> {
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
        "go" => t.starts_with("import "),
        "java" | "kt" => t.starts_with("import "),
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

super::impl_tool!(
    MoveFile,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "move",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for MoveFile {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(MoveFile::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        Some(Path::new(&self.source))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let ctx = crate::types::PermissionContext {
            files: vec![self.source.clone(), self.destination.clone()],
            commands: vec![],
            reason: Some("move file".into()),
        };
        let scopes = vec![
            crate::permissions::canonicalize_scope_path(&self.source),
            crate::permissions::canonicalize_scope_path(&self.destination),
        ];
        Box::pin(std::future::ready(Some(
            super::PermissionScopes::multiple_with_context(scopes, ctx),
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { MoveFile::execute(&self, ctx).await.into() })
    }
}

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
    fn rewrite_imports_only_touches_import_lines() {
        let old = "foo::bar";
        let re = module_path_regex(old).unwrap();
        let content = "use foo::bar::Baz;\nconst COMMENT = \"foo::bar in a string\";\nlet x = foo::bar_value;\n";
        let (count, out) = rewrite_imports(content, &re, "foo::qux", "rs");
        assert_eq!(count, 1);
        assert!(out.contains("use foo::qux::Baz;"));
        assert!(out.contains("\"foo::bar in a string\""));
        assert!(out.contains("foo::bar_value"));
    }

    #[test]
    fn rewrite_imports_avoids_partial_segment_match() {
        let old = "bar";
        let re = module_path_regex(old).unwrap();
        let content = "use foo::bar::Baz;\nuse bar::Thing;\nuse foobar::Other;\n";
        let (count, out) = rewrite_imports(content, &re, "renamed", "rs");
        assert_eq!(count, 1);
        assert!(out.contains("use renamed::Thing;"));
        assert!(out.contains("use foo::bar::Baz;"));
        assert!(out.contains("use foobar::Other;"));
    }
}

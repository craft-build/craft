use std::path::{Path, PathBuf};

use ast_grep_core::NodeMatch;
use ast_grep_language::{LanguageExt, SupportLang};
use craft_tool_macro::Tool;
use serde::Deserialize;
use similar::ChangeTag;

use crate::ToolOutput;

use super::{relative_path, walk_builder_opts};

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct AstGrep {
    #[param(description = "AST pattern with $VAR and $$$BODY metavariables")]
    pattern: String,
    #[param(
        description = "Replacement pattern (omitting = search-only mode). Uses $VAR refs from pattern."
    )]
    rewrite: Option<String>,
    #[param(description = "Language: rust, typescript, tsx, python, go")]
    lang: String,
    #[param(description = "Directory or file to search (default: cwd)")]
    path: Option<String>,
    #[param(description = "Glob patterns to include (e.g. [\"*.rs\", \"src/**\"])")]
    globs: Option<Vec<String>>,
    #[param(description = "Apply replacement (default: dry-run, show diffs only)")]
    apply: Option<bool>,
}

impl AstGrep {
    pub const NAME: &str = "ast_grep";
    pub const DESCRIPTION: &str = include_str!("astgrep.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"pattern": "fn $NAME($$$ARGS)", "lang": "rust"},
  {"pattern": "console.log($MSG)", "rewrite": "tracing::info!($MSG)", "lang": "typescript"},
  {"pattern": "$OBJ.$METHOD($$$ARGS)", "lang": "python", "path": "src/"}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let lang = parse_lang(&self.lang)?;
        let is_replace = self.rewrite.is_some();
        let apply = self.apply.unwrap_or(false) && is_replace;
        let pattern_str = self.pattern.as_str();
        let rewrite_str = self.rewrite.as_deref();

        let search_path = self.path.clone().unwrap_or_else(|| ".".into());
        let globs = self.globs.clone().unwrap_or_default();
        let lang_types = lang.file_types();

        let walk_path = search_path.clone();
        let paths = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>, String> {
            let glob_refs: Vec<&str> = globs.iter().map(|s| s.as_str()).collect();
            let mut builder =
                walk_builder_opts(&walk_path, &glob_refs, true).map_err(|e| e.to_string())?;
            builder.types(lang_types);
            let mut out = Vec::new();
            for entry in builder.build().flatten() {
                if entry.file_type().is_some_and(|ft| ft.is_file()) {
                    out.push(entry.into_path());
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| format!("ast_grep walk failed: {e}"))??;

        let mut results = Vec::new();
        let mut files_scanned = 0u64;
        let mut files_matched = 0u64;

        for path in paths {
            let content = match ctx.fs.read_text_file(&path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            ctx.file_tracker.record_read(&path);

            files_scanned += 1;
            let grep = lang.ast_grep(&content);
            let matches: Vec<NodeMatch<_>> = grep.root().find_all(pattern_str).collect();

            if matches.is_empty() {
                continue;
            }

            files_matched += 1;

            if is_replace {
                let rw = rewrite_str.expect("is_replace implies rewrite is Some");
                let mut grep2 = lang.ast_grep(&content);
                let edits = grep2.root().replace_all(pattern_str, rw);
                for edit in edits.into_iter().rev() {
                    grep2.edit(edit).map_err(|e| e.to_string())?;
                }
                let new_content = grep2.generate();
                let rel = relative_path(&path.to_string_lossy());

                if apply {
                    ctx.file_tracker.check_before_edit(&path)?;
                    let repl_grep = lang.ast_grep(&new_content);
                    if has_error_or_missing(&repl_grep.root()) {
                        results.push(format!(
                            "{rel}: ROLLED BACK — replacement introduces syntax errors"
                        ));
                        continue;
                    }
                    let diff_count = count_changes(&content, &new_content);
                    ctx.fs
                        .write_text_file(&path, &new_content)
                        .await
                        .map_err(|e| format!("write error: {e}"))?;
                    ctx.file_tracker.record_read(&path);
                    results.push(format!("{rel}: {diff_count} replacement(s) applied"));
                } else {
                    let diff = unified_diff(&content, &new_content, &rel);
                    if !diff.is_empty() {
                        results.push(diff);
                    }
                }
            } else {
                let rel = relative_path(&path.to_string_lossy());
                for m in &matches {
                    let pos = m.start_pos();
                    let text = m.text();
                    let preview = truncate_match(&text, 200);
                    results.push(format!("{rel}:{line}: {preview}", line = pos.line() + 1));
                }
            }
        }

        if results.is_empty() {
            return Ok(ToolOutput::Plain(format!(
                "no matches for \"{}\" in {search_path} ({files_scanned} files scanned)",
                self.pattern
            )));
        }

        let mode = if is_replace {
            if apply {
                "replace (applied)"
            } else {
                "replace (dry-run)"
            }
        } else {
            "search"
        };
        let header = format!(
            "{mode}: \"{pattern}\" [{lang}] in {search_path}\n{files_matched}/{files_scanned} files matched\n",
            pattern = self.pattern,
            lang = self.lang,
        );
        let body = truncate_results(&results, 30_000);
        Ok(ToolOutput::Plain(header + &body))
    }

    pub fn start_header(&self) -> String {
        let mode = if self.rewrite.is_some() {
            "replace"
        } else {
            "search"
        };
        format!("ast_grep {mode} {}", self.pattern)
    }
}

fn parse_lang(s: &str) -> Result<SupportLang, String> {
    s.parse::<SupportLang>().map_err(|_| {
        format!(
            "unsupported language \"{s}\"; use: rust, typescript, tsx, python, go, java, c, cpp, ruby, lua, bash, kotlin, swift, c_sharp, elixir, scala, php, html, dart, starlark, nix, zig"
        )
    })
}

fn has_error_or_missing<D: ast_grep_core::Doc>(node: &ast_grep_core::Node<D>) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    node.dfs().any(|n| n.is_error() || n.is_missing())
}

fn count_changes(old: &str, new: &str) -> usize {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .filter(|c| c.tag() == ChangeTag::Delete || c.tag() == ChangeTag::Insert)
        .count()
        .div_ceil(2)
        .max(1)
}

fn truncate_match(text: &str, max: usize) -> String {
    let first_line = text.lines().next().unwrap_or(text);
    if first_line.len() > max {
        let boundary = first_line.floor_char_boundary(max.saturating_sub(3));
        format!("{}...", &first_line[..boundary])
    } else if text.lines().count() > 1 {
        format!("{first_line} ...")
    } else {
        first_line.to_string()
    }
}

fn truncate_results(results: &[String], max_bytes: usize) -> String {
    let mut out = String::new();
    for (i, r) in results.iter().enumerate() {
        if out.len() + r.len() + 1 > max_bytes {
            out.push_str(&format!(
                "\n... ({} more results truncated)",
                results.len() - i
            ));
            break;
        }
        out.push_str(r);
        out.push('\n');
    }
    out
}

fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = String::new();
    for hunk in diff
        .unified_diff()
        .header(&format!("--- {path}"), &format!("+++ {path}"))
        .iter_hunks()
    {
        let _ = std::fmt::write(&mut out, format_args!("{hunk}"));
    }
    out
}

super::impl_tool!(
    AstGrep,
    audience = super::ToolAudience::MAIN,
    kind = "ast_grep",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for AstGrep {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(AstGrep::start_header(self)))
    }
    fn mutable_path(&self) -> Option<&Path> {
        if self.rewrite.is_some() && self.apply.unwrap_or(false) {
            Some(Path::new(self.path.as_deref().unwrap_or(".")))
        } else {
            None
        }
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        let scopes = if self.rewrite.is_some() && self.apply.unwrap_or(false) {
            let path = self.path.clone().unwrap_or_else(|| ".".into());
            let ctx = crate::types::PermissionContext {
                files: vec![path.clone()],
                commands: vec![],
                reason: Some("ast-grep replace".into()),
            };
            Some(super::PermissionScopes::single_with_context(
                crate::permissions::canonicalize_scope_path(&path),
                ctx,
            ))
        } else {
            None
        };
        Box::pin(std::future::ready(scopes))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { AstGrep::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lang_rust() {
        assert!(parse_lang("rust").is_ok());
    }

    #[test]
    fn parse_lang_invalid() {
        assert!(parse_lang("brainfuck").is_err());
    }

    #[test]
    fn truncate_match_short() {
        assert_eq!(truncate_match("fn foo() {}", 200), "fn foo() {}");
    }

    #[test]
    fn truncate_match_multiline() {
        assert_eq!(
            truncate_match("fn foo() {\n  body\n}", 200),
            "fn foo() { ..."
        );
    }

    #[test]
    fn truncate_match_multibyte_safe() {
        let s = "界".repeat(100);
        let result = truncate_match(&s, 10);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() < 100);
    }

    #[test]
    fn count_changes_counts_replacements() {
        let old = "hello\nworld";
        let new = "hello\nearth";
        assert_eq!(count_changes(old, new), 1);
    }

    #[test]
    fn has_error_or_missing_rejects_invalid() {
        let grep = SupportLang::Rust.ast_grep("fn valid() { struct }");
        assert!(has_error_or_missing(&grep.root()));
    }

    #[test]
    fn has_error_or_missing_accepts_valid() {
        let grep = SupportLang::Rust.ast_grep("fn valid() {}");
        assert!(!has_error_or_missing(&grep.root()));
    }

    #[test]
    fn has_error_or_missing_detects_missing_node() {
        let grep = SupportLang::Rust.ast_grep("fn valid() {");
        assert!(has_error_or_missing(&grep.root()));
    }

    #[test]
    fn replace_all_applies_edits() {
        let mut grep = SupportLang::Rust.ast_grep("Vec::new(); Vec::new();");
        let edits = grep.root().replace_all("Vec::new()", "vec![]");
        for edit in edits.into_iter().rev() {
            grep.edit(edit).unwrap();
        }
        assert_eq!(grep.generate(), "vec![]; vec![];");
    }

    #[test]
    fn replace_all_preserves_metavar() {
        let mut grep = SupportLang::Rust.ast_grep("foo(1); foo(2);");
        let edits = grep.root().replace_all("foo($X)", "bar($X)");
        for edit in edits.into_iter().rev() {
            grep.edit(edit).unwrap();
        }
        assert_eq!(grep.generate(), "bar(1); bar(2);");
    }
}

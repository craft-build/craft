use std::path::Path;

use ast_grep_language::SupportLang;
use craft_tool_macro::Tool;
use serde::Deserialize;
use similar::ChangeTag;

use argosy::codetools::astgrep::{self, AstgrepParams};

use crate::ToolOutput;

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
        let params = AstgrepParams {
            pattern: self.pattern.clone(),
            rewrite: self.rewrite.clone(),
            lang: self.lang.clone(),
            path: self.path.clone(),
            globs: self.globs.clone(),
            apply: self.apply,
        };
        let report = astgrep::run(&ctx.code_tools, params).map_err(|e| e.to_string())?;
        let text = match report.mode {
            "diff" => report.text.replacen("diff:", "replace (dry-run):", 1),
            "apply" => report.text.replacen("apply:", "replace (applied):", 1),
            _ => report.text,
        };
        Ok(ToolOutput::Plain(text))
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

pub(crate) fn parse_lang(s: &str) -> Result<SupportLang, String> {
    s.parse::<SupportLang>().map_err(|_| {
        format!(
            "unsupported language \"{s}\"; use: rust, typescript, tsx, python, go, java, c, cpp, ruby, lua, bash, kotlin, swift, c_sharp, elixir, scala, php, html, dart, starlark, nix, zig"
        )
    })
}

pub(crate) fn has_error_or_missing<D: ast_grep_core::Doc>(node: &ast_grep_core::Node<D>) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    node.dfs().any(|n| n.is_error() || n.is_missing())
}

pub(crate) fn count_changes(old: &str, new: &str) -> usize {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .filter(|c| c.tag() == ChangeTag::Delete || c.tag() == ChangeTag::Insert)
        .count()
        .div_ceil(2)
        .max(1)
}

pub(crate) fn unified_diff(old: &str, new: &str, path: &str) -> String {
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
                crate::permissions::normalize_scope_path(&path),
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
    use ast_grep_language::LanguageExt;

    #[test]
    fn parse_lang_rust() {
        assert!(parse_lang("rust").is_ok());
    }

    #[test]
    fn parse_lang_invalid() {
        assert!(parse_lang("brainfuck").is_err());
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

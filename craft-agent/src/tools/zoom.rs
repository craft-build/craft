use std::path::Path;

use crate::ToolOutput;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::outline::{self, LangId};
use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Zoom {
    #[param(description = "Absolute path to the file", alias = "file_path")]
    path: String,
    #[param(description = "Symbol name to zoom into (function, struct, class, heading, etc.)")]
    symbol: Option<String>,
    #[param(description = "Start line (1-indexed) for line-range mode")]
    start_line: Option<usize>,
    #[param(description = "End line (1-indexed) for line-range mode")]
    end_line: Option<usize>,
    #[param(description = "Lines of context around the symbol body (default 3)")]
    context_lines: Option<usize>,
}

impl Zoom {
    pub const NAME: &str = "zoom";
    pub const DESCRIPTION: &str = include_str!("zoom.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs", "symbol": "main"},
  {"path": "/project/README.md", "symbol": "Installation"},
  {"path": "/project/src/lib.rs", "start_line": 10, "end_line": 25}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let p = Path::new(&path);

        if !p.is_file() {
            return Err(format!("path is not a file: {}", relative_path(&path)));
        }

        let content = ctx.fs.read_text_file(p).await?;
        ctx.file_tracker.record_read(p);

        let context = self.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES);

        if let Some(symbol_name) = &self.symbol {
            return self.zoom_by_symbol(&content, &path, symbol_name, context);
        }

        if let (Some(start), Some(end)) = (self.start_line, self.end_line) {
            return self.zoom_by_range(&content, &path, start, end, context);
        }

        Err("provide either `symbol` or both `start_line` and `end_line`".into())
    }

    fn zoom_by_symbol(
        &self,
        content: &str,
        path: &str,
        symbol_name: &str,
        context: usize,
    ) -> Result<ToolOutput, String> {
        let p = Path::new(path);
        let lang = LangId::from_path(p);

        let Some(lang) = lang else {
            return self.text_search(content, path, symbol_name, context);
        };

        let symbols = outline::extract_symbols(content, lang);
        let matches: Vec<_> = symbols.iter().filter(|s| s.name == symbol_name).collect();

        if matches.is_empty() {
            return self.text_search(content, path, symbol_name, context);
        }

        if matches.len() > 1 {
            let candidates: Vec<String> = matches
                .iter()
                .map(|s| {
                    format!(
                        "{}::{}:{} ({}-{})",
                        s.kind.label(),
                        s.name,
                        s.range.start_row + 1,
                        s.range.start_row + 1,
                        s.range.end_row + 1
                    )
                })
                .collect();
            return Err(format!(
                "ambiguous symbol \"{symbol_name}\"; candidates:\n{}",
                candidates.join("\n")
            ));
        }

        let sym = &matches[0];
        let start = sym.range.start_row.saturating_sub(context);
        let end = (sym.range.end_row + context).min(content.lines().count() - 1);

        let snippet = extract_lines(content, start, end);
        let header = format!(
            "{} {} ({}:{}-{})",
            sym.kind.label(),
            sym.name,
            sym.range.start_row + 1,
            sym.range.end_row + 1,
            relative_path(path)
        );

        Ok(ToolOutput::Plain(format!("{header}\n{snippet}")))
    }

    fn zoom_by_range(
        &self,
        content: &str,
        path: &str,
        start_line: usize,
        end_line: usize,
        context: usize,
    ) -> Result<ToolOutput, String> {
        let total = content.lines().count();
        if start_line == 0 || start_line > total {
            return Err(format!("start_line {start_line} out of range (1-{total})"));
        }
        if end_line < start_line {
            return Err(format!(
                "end_line {end_line} must be >= start_line {start_line}"
            ));
        }

        let start = start_line.saturating_sub(1).saturating_sub(context);
        let end = (end_line - 1 + context).min(total - 1);

        let snippet = extract_lines(content, start, end);
        let header = format!("lines {}-{} {}", start + 1, end + 1, relative_path(path));

        Ok(ToolOutput::Plain(format!("{header}\n{snippet}")))
    }

    fn text_search(
        &self,
        content: &str,
        path: &str,
        symbol_name: &str,
        context: usize,
    ) -> Result<ToolOutput, String> {
        let lines: Vec<&str> = content.lines().collect();
        let mut matches: Vec<usize> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let is_heading = (trimmed.starts_with('#') || trimmed.starts_with("<h"))
                && trimmed.contains(symbol_name);
            let is_def = SYMBOL_PREFIXES
                .iter()
                .any(|p| trimmed.starts_with(&format!("{p}{symbol_name}")))
                || trimmed == symbol_name;

            if is_heading || is_def {
                matches.push(i);
            }
        }

        if matches.is_empty() {
            return Err(format!(
                "symbol \"{symbol_name}\" not found in {}",
                relative_path(path)
            ));
        }

        if matches.len() > 1 {
            let candidates: Vec<String> =
                matches.iter().map(|&i| format!("line {}", i + 1)).collect();
            return Err(format!(
                "ambiguous symbol \"{symbol_name}\"; found at:\n{}",
                candidates.join("\n")
            ));
        }

        let match_line = matches[0];
        let start = match_line.saturating_sub(context);
        let end = (match_line + context).min(lines.len() - 1);

        let snippet = extract_lines(content, start, end);
        let header = format!(
            "text match at line {} {}",
            match_line + 1,
            relative_path(path)
        );

        Ok(ToolOutput::Plain(format!("{header}\n{snippet}")))
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

const DEFAULT_CONTEXT_LINES: usize = 3;
const SYMBOL_PREFIXES: &[&str] = &[
    "",
    "fn ",
    "def ",
    "function ",
    "class ",
    "struct ",
    "impl ",
    "enum ",
    "pub fn ",
    "pub struct ",
];

fn extract_lines(content: &str, start: usize, end: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = start.min(lines.len().saturating_sub(1));
    let end = end.min(lines.len().saturating_sub(1));

    let width = format!("{}", end + 1).len();
    let mut out = String::new();
    for (i, line) in lines[start..=end].iter().enumerate() {
        let ln = start + i + 1;
        let _ = std::fmt::write(&mut out, format_args!("{ln:>width$} | {line}\n"));
    }
    out
}

super::impl_tool!(
    Zoom,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::GENERAL_SUB
        | super::ToolAudience::INTERPRETER,
    kind = "zoom",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for Zoom {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Zoom::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Zoom::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_lines_numbered() {
        let content = "a\nb\nc\nd\ne";
        let result = extract_lines(content, 1, 3);
        assert!(result.contains("2 | b"));
        assert!(result.contains("3 | c"));
        assert!(result.contains("4 | d"));
    }

    #[test]
    fn zoom_by_range_basic() {
        let zoom = Zoom {
            path: "/test.rs".into(),
            symbol: None,
            start_line: Some(2),
            end_line: Some(4),
            context_lines: Some(0),
        };
        let content = "line1\nline2\nline3\nline4\nline5";
        let result = zoom.zoom_by_range(content, "/test.rs", 2, 4, 0).unwrap();
        assert!(result.as_text().contains("2 | line2"));
        assert!(result.as_text().contains("4 | line4"));
    }

    #[test]
    fn zoom_by_symbol_rust_fn() {
        let zoom = Zoom {
            path: "/test.rs".into(),
            symbol: Some("greet".into()),
            start_line: None,
            end_line: None,
            context_lines: Some(0),
        };
        let content = "fn greet() {\n    println!(\"hi\");\n}\nfn other() {}";
        let result = zoom
            .zoom_by_symbol(content, "/test.rs", "greet", 0)
            .unwrap();
        let text = result.as_text();
        assert!(text.contains("greet"));
        assert!(text.contains("1 |"));
    }

    #[test]
    fn ambiguous_symbol_returns_candidates() {
        let zoom = Zoom {
            path: "/test.rs".into(),
            symbol: Some("foo".into()),
            start_line: None,
            end_line: None,
            context_lines: Some(0),
        };
        let content = "struct Foo {\n    x: i32,\n}\nimpl Foo {\n    fn foo() {}\n}\nfn foo() {}";
        let result = zoom.zoom_by_symbol(content, "/test.rs", "foo", 0);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn missing_symbol_returns_error() {
        let zoom = Zoom {
            path: "/test.txt".into(),
            symbol: Some("nonexistent".into()),
            start_line: None,
            end_line: None,
            context_lines: Some(0),
        };
        let content = "nothing here";
        let result = zoom.text_search(content, "/test.txt", "nonexistent", 0);
        assert!(result.is_err());
    }

    #[test]
    fn zoom_range_out_of_bounds() {
        let zoom = Zoom {
            path: "/test.rs".into(),
            symbol: None,
            start_line: Some(100),
            end_line: Some(110),
            context_lines: None,
        };
        let content = "only\nthree\nlines";
        let result = zoom.zoom_by_range(content, "/test.rs", 100, 110, 0);
        assert!(result.is_err());
    }
}

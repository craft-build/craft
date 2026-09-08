use craft_tool_macro::Tool;
use serde::Deserialize;

use argosy::codetools::zoom::{self, ZoomParams};

use crate::ToolOutput;

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
        let params = ZoomParams {
            path: self.path.clone(),
            symbol: self.symbol.clone(),
            start_line: self.start_line,
            end_line: self.end_line,
            context_lines: self.context_lines,
        };
        let report = zoom::run(&ctx.code_tools, params)
            .map_err(|e| normalize_ambiguous_error(&e.to_string()))?;
        let resolved = super::resolve_path(&self.path)?;
        ctx.record_read(std::path::Path::new(&resolved));
        Ok(ToolOutput::Plain(report.text))
    }

    pub fn start_header(&self) -> String {
        super::relative_path(&self.path)
    }
}

const AMBIGUOUS_MARKER: &str = "; candidates:\n";

fn normalize_ambiguous_error(err: &str) -> String {
    let Some(idx) = err.find(AMBIGUOUS_MARKER) else {
        return err.to_string();
    };
    let (head, candidates) = err.split_at(idx + AMBIGUOUS_MARKER.len());
    let re = regex::Regex::new(r"^(\w+)::(.+) \(lines (\d+)-(\d+)\)$").unwrap();
    let lines: Vec<String> = candidates
        .lines()
        .map(|l| {
            re.replace(l, "$1::$2:$3 ($3-$4)")
                .parse()
                .unwrap_or_else(|_| l.to_string())
        })
        .collect();
    format!("{head}{}", lines.join("\n"))
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
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    fn run_zoom(
        dir: &std::path::Path,
        name: &str,
        args: &[(&str, serde_json::Value)],
    ) -> Result<String, String> {
        let mut json = serde_json::Map::new();
        json.insert("path".into(), dir.join(name).to_string_lossy().into());
        for (k, v) in args {
            json.insert((*k).into(), v.clone());
        }
        let tool: Zoom = serde_json::from_value(json.into()).unwrap();
        futures::executor::block_on(tool.execute(&stub_ctx(&AgentMode::Build)))
            .map(|out| out.as_text().to_string())
    }

    #[test]
    fn zoom_by_range_basic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.rs"),
            "line1\nline2\nline3\nline4\nline5",
        )
        .unwrap();
        let text = run_zoom(
            dir.path(),
            "test.rs",
            &[
                ("start_line", serde_json::json!(2)),
                ("end_line", serde_json::json!(4)),
                ("context_lines", serde_json::json!(0)),
            ],
        )
        .unwrap();
        assert!(text.contains("2 | line2"));
        assert!(text.contains("4 | line4"));
    }

    #[test]
    fn zoom_by_symbol_rust_fn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.rs"),
            "fn greet() {\n    println!(\"hi\");\n}\nfn other() {}",
        )
        .unwrap();
        let text = run_zoom(
            dir.path(),
            "test.rs",
            &[
                ("symbol", serde_json::json!("greet")),
                ("context_lines", serde_json::json!(0)),
            ],
        )
        .unwrap();
        assert!(text.contains("greet"));
        assert!(text.contains("1 |"));
    }

    #[test]
    fn ambiguous_symbol_returns_candidates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.rs"),
            "struct Foo {\n    x: i32,\n}\nimpl Foo {\n    fn foo() {}\n}\nfn foo() {}",
        )
        .unwrap();
        let err = run_zoom(
            dir.path(),
            "test.rs",
            &[
                ("symbol", serde_json::json!("foo")),
                ("context_lines", serde_json::json!(0)),
            ],
        )
        .unwrap_err();
        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn missing_symbol_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "nothing here").unwrap();
        let err = run_zoom(
            dir.path(),
            "test.txt",
            &[
                ("symbol", serde_json::json!("nonexistent")),
                ("context_lines", serde_json::json!(0)),
            ],
        )
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn ambiguous_candidates_keep_craft_format() {
        let err = normalize_ambiguous_error(
            "ambiguous symbol \"foo\"; candidates:\nfn::foo (lines 3-5)\nme::foo (lines 8-9)",
        );
        assert!(
            err.contains("fn::foo:3 (3-5)") && err.contains("me::foo:8 (8-9)"),
            "got: {err}"
        );
    }

    #[test]
    fn zoom_range_out_of_bounds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "only\nthree\nlines").unwrap();
        let err = run_zoom(
            dir.path(),
            "test.rs",
            &[
                ("start_line", serde_json::json!(100)),
                ("end_line", serde_json::json!(110)),
            ],
        )
        .unwrap_err();
        assert!(err.contains("out of range"));
    }
}

use craft_tool_macro::Tool;
use serde::Deserialize;

use argosy::codetools::callgraph::{self, CallgraphParams};

use crate::ToolOutput;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Callgraph {
    #[param(description = "Operation: call_tree, callers, or impact")]
    op: String,
    #[param(description = "File path")]
    path: String,
    #[param(description = "Symbol name (function/method/struct)")]
    symbol: String,
    #[param(description = "Max depth for call_tree (default 5)")]
    depth: Option<usize>,
}

impl Callgraph {
    pub const NAME: &str = "callgraph";
    pub const DESCRIPTION: &str = include_str!("callgraph.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"op": "call_tree", "path": "src/main.rs", "symbol": "run"},
  {"op": "callers", "path": "src/lib.rs", "symbol": "Config"},
  {"op": "impact", "path": "src/lib.rs", "symbol": "parse_args"}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let params = CallgraphParams {
            op: self.op.clone(),
            path: self.path.clone(),
            symbol: self.symbol.clone(),
            depth: self.depth,
        };
        let report = callgraph::run(&ctx.code_tools, params).map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(report.text))
    }

    pub fn start_header(&self) -> String {
        format!("callgraph {} {}", self.op, self.symbol)
    }
}

super::impl_tool!(Callgraph, kind = "callgraph", tier = super::ToolTier::Core,);

impl super::ToolInvocation for Callgraph {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Callgraph::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Callgraph::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    const RUST_SRC: &str = r#"
fn main() {
    foo();
    bar();
}

fn foo() {
    baz();
    external();
}

fn bar() {
    baz();
}

fn baz() {
    println!("hi");
}
"#;

    fn run(op: &str, symbol: &str) -> Result<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, RUST_SRC).unwrap();
        let tool = Callgraph {
            op: op.to_string(),
            path: path.to_string_lossy().into_owned(),
            symbol: symbol.to_string(),
            depth: Some(5),
        };
        futures::executor::block_on(tool.execute(&stub_ctx(&AgentMode::Build)))
            .map(|out| out.as_text().to_string())
    }

    #[test]
    fn call_tree_builds_hierarchy() {
        let text = run("call_tree", "main").unwrap();
        assert!(text.contains("main (line 2)"), "got: {text}");
        assert!(text.contains("foo"));
        assert!(text.contains("bar"));
    }

    #[test]
    fn find_callers_finds_direct() {
        let text = run("callers", "baz").unwrap();
        assert!(text.contains("foo"), "got: {text}");
        assert!(text.contains("bar"));
    }

    #[test]
    fn find_impact_traverses_transitively() {
        let text = run("impact", "baz").unwrap();
        assert!(text.contains("foo"), "got: {text}");
        assert!(text.contains("bar"));
        assert!(text.contains("main"));
    }

    #[test]
    fn find_symbol_rejects_unknown() {
        let err = run("call_tree", "nonexistent").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn unknown_op_errors() {
        let err = run("nope", "main").unwrap_err();
        assert!(err.contains("unknown op"));
    }
}

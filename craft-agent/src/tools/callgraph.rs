use std::collections::HashSet;

use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::{
    outline::{LangId, Range, Symbol, extract_symbols},
    relative_path,
};

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
        let lang = LangId::from_path(self.path.as_ref())
            .ok_or_else(|| format!("unsupported file type: {}", relative_path(&self.path)))?;

        let resolved = super::resolve_path(&self.path)?;
        let p = std::path::Path::new(&resolved);
        let content = ctx.fs.read_text_file(p).await?;
        ctx.file_tracker.record_read(p);

        let symbols = extract_symbols(&content, lang);
        let calls = extract_calls(&content, lang);

        let target = find_symbol(&symbols, &self.symbol)?;

        match self.op.as_str() {
            "call_tree" => {
                let max_depth = self.depth.unwrap_or(5);
                let tree = build_call_tree(target, &symbols, &calls, max_depth);
                Ok(ToolOutput::Plain(render_call_tree(&tree, 0)))
            }
            "callers" => {
                let callers = find_callers(target, &symbols, &calls);
                Ok(ToolOutput::Plain(render_symbol_list(
                    "callers",
                    &target.name,
                    &callers,
                )))
            }
            "impact" => {
                let impacted = find_impact(target, &symbols, &calls);
                Ok(ToolOutput::Plain(render_symbol_list(
                    "impact",
                    &target.name,
                    &impacted,
                )))
            }
            _ => Err(format!(
                "unknown op \"{}\"; use call_tree, callers, or impact",
                self.op
            )),
        }
    }

    pub fn start_header(&self) -> String {
        format!("callgraph {} {}", self.op, self.symbol)
    }
}

fn find_symbol<'a>(symbols: &'a [Symbol], name: &str) -> Result<&'a Symbol, String> {
    let matches: Vec<&Symbol> = symbols.iter().filter(|s| s.name == name).collect();
    match matches.len() {
        0 => {
            let candidates: Vec<String> = symbols
                .iter()
                .filter(|s| s.name.contains(name))
                .map(|s| s.name.clone())
                .take(10)
                .collect();
            let hint = if candidates.is_empty() {
                String::new()
            } else {
                format!(" similar: {}", candidates.join(", "))
            };
            Err(format!("symbol \"{name}\" not found in file.{hint}"))
        }
        1 => Ok(matches[0]),
        n => {
            let disambig: Vec<String> = matches
                .iter()
                .map(|s| format!("{} (line {})", s.name, s.range.start_row + 1))
                .collect();
            Err(format!(
                "symbol \"{name}\" is ambiguous ({n} matches): {}",
                disambig.join(", ")
            ))
        }
    }
}

#[derive(Debug, Clone)]
struct RawCall {
    name: String,
    line: usize,
}

fn extract_calls(content: &str, lang: LangId) -> Vec<RawCall> {
    let source = content.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        tracing::error!(
            lang = lang.name(),
            "callgraph parser rejected language abi, skipping"
        );
        return Vec::new();
    }

    let Some(tree) = parser.parse(source, None) else {
        tracing::error!(
            lang = lang.name(),
            "callgraph parser returned no tree, skipping"
        );
        return Vec::new();
    };
    let root = tree.root_node();

    let mut calls = Vec::new();
    walk_for_calls(root, source, lang, &mut calls);
    calls
}

fn walk_for_calls(root: tree_sitter::Node, source: &[u8], lang: LangId, calls: &mut Vec<RawCall>) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if is_call_node(node.kind(), lang)
            && let Some(name) = extract_call_name(node, source, lang)
        {
            calls.push(RawCall {
                name,
                line: node.start_position().row,
            });
        }
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }
}

fn is_call_node(kind: &str, lang: LangId) -> bool {
    match lang {
        LangId::Rust => kind == "call_expression",
        LangId::TypeScript => kind == "call_expression",
        LangId::Python => kind == "call",
        LangId::Go => kind == "call_expression",
        LangId::Java => kind == "method_invocation" || kind == "class_instance_creation_expression",
        LangId::C | LangId::Cpp => kind == "call_expression",
        LangId::Ruby => kind == "call",
        LangId::Lua => kind == "function_call",
        _ => kind == "call_expression",
    }
}

fn extract_call_name(node: tree_sitter::Node, source: &[u8], lang: LangId) -> Option<String> {
    let func_node = match lang {
        LangId::Python => node.child_by_field_name("function"),
        _ => node.child_by_field_name("function"),
    };

    let func_node = func_node?;

    let text = func_node.utf8_text(source).ok()?;
    let name = if text.contains('.') {
        text.rsplit('.').next().unwrap_or(text).to_string()
    } else {
        text.to_string()
    };

    if name.is_empty() || name.starts_with('$') || name.starts_with('<') {
        None
    } else {
        Some(name)
    }
}

fn calls_in_range(calls: &[RawCall], range: &Range) -> Vec<usize> {
    calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.line >= range.start_row && c.line <= range.end_row)
        .map(|(i, _)| i)
        .collect()
}

#[derive(Debug)]
struct CallTreeNode {
    name: String,
    line: usize,
    children: Vec<CallTreeNode>,
}

fn build_call_tree(
    symbol: &Symbol,
    symbols: &[Symbol],
    calls: &[RawCall],
    max_depth: usize,
) -> CallTreeNode {
    build_call_tree_inner(symbol, symbols, calls, max_depth, &mut HashSet::new())
}

fn build_call_tree_inner(
    symbol: &Symbol,
    symbols: &[Symbol],
    calls: &[RawCall],
    remaining: usize,
    visited: &mut HashSet<String>,
) -> CallTreeNode {
    let in_scope_calls = calls_in_range(calls, &symbol.range);

    let mut children = Vec::new();
    if remaining > 0 && visited.insert(symbol.name.clone()) {
        for &idx in &in_scope_calls {
            let call = &calls[idx];
            if let Some(called) = symbols.iter().find(|s| s.name == call.name) {
                children.push(build_call_tree_inner(
                    called,
                    symbols,
                    calls,
                    remaining - 1,
                    visited,
                ));
            } else {
                children.push(CallTreeNode {
                    name: call.name.clone(),
                    line: call.line,
                    children: Vec::new(),
                });
            }
        }
        visited.remove(&symbol.name);
    }

    CallTreeNode {
        name: symbol.name.clone(),
        line: symbol.range.start_row,
        children,
    }
}

fn find_callers<'a>(target: &Symbol, symbols: &'a [Symbol], calls: &[RawCall]) -> Vec<&'a Symbol> {
    symbols
        .iter()
        .filter(|s| {
            s.range.start_row != target.range.start_row
                && calls_in_range(calls, &s.range)
                    .iter()
                    .any(|&i| calls[i].name == target.name)
        })
        .collect()
}

fn find_impact<'a>(target: &Symbol, symbols: &'a [Symbol], calls: &[RawCall]) -> Vec<&'a Symbol> {
    let mut impacted = Vec::new();
    let mut queue = vec![target.name.clone()];
    let mut seen: HashSet<String> = [target.name.clone()].into_iter().collect();

    while let Some(name) = queue.pop() {
        let callers: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| {
                !seen.contains(&s.name)
                    && calls_in_range(calls, &s.range)
                        .iter()
                        .any(|&i| calls[i].name == name)
            })
            .collect();

        for caller in &callers {
            seen.insert(caller.name.clone());
            queue.push(caller.name.clone());
            impacted.push(*caller);
        }
    }

    impacted
}

fn render_call_tree(node: &CallTreeNode, depth: usize) -> String {
    let mut out = String::new();
    render_call_tree_inner(node, depth, &mut out);
    out
}

fn render_call_tree_inner(node: &CallTreeNode, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let _ = std::fmt::write(
        out,
        format_args!("{}{} (line {})\n", indent, node.name, node.line + 1),
    );
    for child in &node.children {
        render_call_tree_inner(child, depth + 1, out);
    }
}

fn render_symbol_list(label: &str, target_name: &str, symbols: &[&Symbol]) -> String {
    let mut out = format!("{label} of \"{target_name}\"\n");
    if symbols.is_empty() {
        out.push_str("  (none found in this file)\n");
    } else {
        for s in symbols {
            let _ = std::fmt::write(
                &mut out,
                format_args!(
                    "  {} {} (line {})\n",
                    s.kind.label(),
                    s.name,
                    s.range.start_row + 1
                ),
            );
        }
    }
    out
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

    fn rust_symbols() -> Vec<Symbol> {
        let code = r#"
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
        extract_symbols(code, LangId::Rust)
    }

    fn rust_calls() -> Vec<RawCall> {
        let code = r#"
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
        extract_calls(code, LangId::Rust)
    }

    #[test]
    fn find_symbol_returns_matching() {
        let syms = rust_symbols();
        let result = find_symbol(&syms, "main");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name, "main");
    }

    #[test]
    fn find_symbol_rejects_unknown() {
        let syms = rust_symbols();
        let result = find_symbol(&syms, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn find_callers_finds_direct() {
        let syms = rust_symbols();
        let calls = rust_calls();
        let target = find_symbol(&syms, "baz").unwrap();
        let callers = find_callers(target, &syms, &calls);
        let names: Vec<&str> = callers.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"foo"),
            "expected foo in callers, got: {names:?}"
        );
        assert!(
            names.contains(&"bar"),
            "expected bar in callers, got: {names:?}"
        );
    }

    #[test]
    fn find_impact_traverses_transitively() {
        let syms = rust_symbols();
        let calls = rust_calls();
        let target = find_symbol(&syms, "baz").unwrap();
        let impacted = find_impact(target, &syms, &calls);
        let names: Vec<&str> = impacted.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"foo"), "expected foo in impact");
        assert!(names.contains(&"bar"), "expected bar in impact");
        assert!(names.contains(&"main"), "expected main in impact");
    }

    #[test]
    fn call_tree_builds_hierarchy() {
        let syms = rust_symbols();
        let calls = rust_calls();
        let target = find_symbol(&syms, "main").unwrap();
        let tree = build_call_tree(target, &syms, &calls, 5);
        assert_eq!(tree.name, "main");
        let child_names: Vec<&str> = tree.children.iter().map(|c| c.name.as_str()).collect();
        assert!(
            child_names.contains(&"foo"),
            "expected foo in call tree children"
        );
        assert!(
            child_names.contains(&"bar"),
            "expected bar in call tree children"
        );
    }
}

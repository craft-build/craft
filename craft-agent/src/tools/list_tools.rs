use serde_json::Value;
use tracing::info;

use craft_tool_macro::Tool;

use crate::ToolOutput;
use crate::tools::DescriptionContext;
use crate::tools::registry::{ToolRegistry, ToolTier};

use super::ToolFilter;

#[derive(Tool, Debug, Clone, serde::Deserialize)]
pub struct ListTools {
    #[param(
        description = "Optional tool name to inspect. Returns the full input schema and enables \
                       the tool for the rest of the session. Omit to list every tool with a \
                       short description."
    )]
    detail: Option<String>,
}

const DETAIL_PROMOTED: &str = "enabled for the rest of this session";
const UNKNOWN_TOOL: &str = "unknown tool";

impl ListTools {
    pub const NAME: &str = "list_tools";
    pub const DESCRIPTION: &str = include_str!("list_tools.md");
    pub const EXAMPLES: Option<&str> = None;

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        if let Some(name) = self.detail.as_deref() {
            return Ok(promote_and_detail(ctx, name));
        }
        Ok(list_all(ctx))
    }
}

fn promote_and_detail(ctx: &super::ToolContext, name: &str) -> ToolOutput {
    let schema = schema_for(ctx, name);
    match schema {
        Some(schema) => {
            ctx.promoted.promote(name);
            info!(tool = %name, "tool promoted via list_tools");
            let pretty = format_pretty_schema(&schema);
            ToolOutput::Plain(format!(
                "{name}: {DETAIL_PROMOTED}.\n\nInput schema:\n{pretty}"
            ))
        }
        None => ToolOutput::Plain(format!(
            "{UNKNOWN_TOOL}: {name}. Call list_tools() to see available tools."
        )),
    }
}

fn schema_for(ctx: &super::ToolContext, name: &str) -> Option<Value> {
    let registry = ToolRegistry::native();
    if let Some(entry) = registry.get(name) {
        return Some(entry.tool.schema());
    }
    if let Some(handle) = &ctx.mcp {
        let mut tools = Value::Array(Vec::new());
        handle.extend_tools(&mut tools);
        if let Some(arr) = tools.as_array() {
            for def in arr {
                if def.get("name").and_then(|v| v.as_str()) == Some(name) {
                    return def.get("input_schema").cloned();
                }
            }
        }
    }
    None
}

fn format_pretty_schema(schema: &Value) -> String {
    match serde_json::to_string_pretty(schema) {
        Ok(s) => s,
        Err(_) => schema.to_string(),
    }
}

fn list_all(ctx: &super::ToolContext) -> ToolOutput {
    let mut entries: Vec<(String, String, ToolTier)> = Vec::new();

    let registry = ToolRegistry::native();
    let filter = ToolFilter::All;
    let dctx = DescriptionContext { filter: &filter };
    for entry in registry.iter().iter() {
        let name = entry.name().to_string();
        if name == ListTools::NAME {
            continue;
        }
        let desc = first_line_str(&entry.tool.description(&dctx));
        let tier = entry.tool.tier();
        entries.push((name, desc, tier));
    }

    if let Some(handle) = &ctx.mcp {
        let mut tools = Value::Array(Vec::new());
        handle.extend_tools(&mut tools);
        if let Some(arr) = tools.as_array() {
            for def in arr {
                if let Some(name) = def.get("name").and_then(|v| v.as_str()) {
                    let desc = def
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(first_line_str)
                        .unwrap_or_default();
                    entries.push((name.to_string(), desc, ToolTier::Extended));
                }
            }
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let active = |name: &str| is_active(ctx, name);
    let mut core: Vec<&(String, String, ToolTier)> = Vec::new();
    let mut extended: Vec<&(String, String, ToolTier)> = Vec::new();
    for e in &entries {
        if active(&e.0) {
            core.push(e);
        } else {
            extended.push(e);
        }
    }

    let mut out = String::new();
    out.push_str("Available tools:\n\n");
    out.push_str("Already available (no promotion needed):\n");
    if core.is_empty() {
        out.push_str("  (none)\n");
    }
    for (name, desc, _) in &core {
        out.push_str(&format!("- {name}: {desc}\n"));
    }
    out.push_str("\nNot yet available — call list_tools(detail=\"<name>\") to enable and inspect:\n");
    if extended.is_empty() {
        out.push_str("  (none)\n");
    }
    for (name, desc, _) in &extended {
        out.push_str(&format!("- {name}: {desc}\n"));
    }
    ToolOutput::Plain(out)
}

fn is_active(ctx: &super::ToolContext, name: &str) -> bool {
    if !ctx.dynamic.enabled {
        return true;
    }
    ctx.dynamic.core.contains(name) || ctx.promoted.contains(name)
}

fn first_line_str(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

super::impl_tool!(
    ListTools,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::RESEARCH_SUB
        | super::ToolAudience::GENERAL_SUB,
    kind = "meta",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for ListTools {
    fn start_header(&self) -> super::HeaderFuture {
        let label = match &self.detail {
            Some(name) => format!("list_tools: {name}"),
            None => "list_tools".to_string(),
        };
        super::HeaderFuture::Ready(super::HeaderResult::plain(label))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { ListTools::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_trims() {
        assert_eq!(first_line_str("hello world\nsecond"), "hello world");
        assert_eq!(first_line_str("  spaced  "), "spaced");
        assert_eq!(first_line_str(""), "");
    }
}

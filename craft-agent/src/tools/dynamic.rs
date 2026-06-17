//! Dynamic tool discovery. All builtin tools (native + bundled Lua plugins)
//! are advertised every turn; only **MCP server** tools start hidden and must be
//! promoted via the `list_tools` tool. The active set advertised to the provider
//! is `builtins ∪ promoted_mcp`, rebuilt each turn so a mid-session promotion
//! shows up on the very next request.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use craft_providers::Model;

use crate::mcp::McpHandle;
use crate::template::Vars;
use crate::{AgentConfig, tools};

use super::registry::ToolRegistry;
use super::{DescriptionContext, ToolFilter};

/// Per-session handle for tools the model has promoted via `list_tools`. Cloning is
/// cheap (one `Arc`), so it flows through `ToolContext` into the `list_tools` tool.
#[derive(Clone)]
pub struct PromotedTools(Arc<ArcSwap<HashSet<String>>>);

impl PromotedTools {
    pub fn new() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(HashSet::new())))
    }

    pub fn promote(&self, name: &str) {
        let owned = name.to_string();
        self.0.rcu(|current| {
            if current.contains(&owned) {
                return Arc::clone(current);
            }
            let mut next = (**current).clone();
            next.insert(owned.clone());
            Arc::new(next)
        });
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.load().contains(name)
    }

    pub fn snapshot(&self) -> Arc<HashSet<String>> {
        self.0.load_full()
    }
}

impl Default for PromotedTools {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolved dynamic-discovery state for a session: whether it is on.
#[derive(Clone)]
pub struct DynamicContext {
    pub enabled: bool,
}

impl DynamicContext {
    /// `enabled = false` advertises the full toolset (current pre-discovery behavior).
    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    pub fn from_config(config: &AgentConfig) -> Self {
        Self {
            enabled: config.dynamic_tools.enabled,
        }
    }
}

/// Everything the agent needs to rebuild the advertised tool list each turn.
#[derive(Clone)]
pub struct ToolBuild {
    pub vars: Vars,
    pub excluded: Vec<&'static str>,
    pub mcp: Option<McpHandle>,
}

/// Build the tool definitions sent to the provider this turn. When dynamic discovery
/// is on, MCP tools are filtered to the promoted set; all builtins are always kept.
/// Otherwise the full toolset is returned unchanged.
pub fn build_active_tools(
    build: &ToolBuild,
    model: &Model,
    config: &AgentConfig,
    dynamic: &DynamicContext,
    promoted: &PromotedTools,
) -> Value {
    let filter = ToolFilter::from_config(config, &build.excluded);
    let ctx = DescriptionContext { filter: &filter };
    let mut tools =
        ToolRegistry::native().definitions(&build.vars, &ctx, model.supports_tool_examples());
    let mcp_names = build
        .mcp
        .as_ref()
        .map(|h| h.tool_names())
        .unwrap_or_default();
    if let Some(handle) = &build.mcp {
        handle.extend_tools(&mut tools);
    }
    if dynamic.enabled {
        let promoted_snap = promoted.snapshot();
        tools = filter_to_active(&tools, &mcp_names, &promoted_snap);
    }
    tools
}

/// Keep a tool definition iff it is a builtin (not MCP), already promoted, or the
/// discovery tool itself. Definitions without a parseable `name` are kept defensively.
pub fn filter_to_active(tools: &Value, mcp_names: &[String], promoted: &HashSet<String>) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let mcp_set: HashSet<&str> = mcp_names.iter().map(|s| s.as_str()).collect();
    let kept: Vec<Value> = arr
        .iter()
        .filter(|def| {
            let Some(name) = def.get("name").and_then(|v| v.as_str()) else {
                return true;
            };
            name == tools::LIST_TOOLS_TOOL_NAME
                || !mcp_set.contains(name)
                || promoted.contains(name)
        })
        .cloned()
        .collect();
    Value::Array(kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str) -> Value {
        json!({"name": name, "description": name, "input_schema": {}})
    }

    #[test]
    fn filter_keeps_builtins_and_promoted_mcp() {
        let tools = json!([def("read"), def("review"), def("mcp__search")]);
        let mcp_names: Vec<String> = ["mcp__search"].iter().map(|s| s.to_string()).collect();
        let promoted: HashSet<String> = ["mcp__search"].iter().map(|s| s.to_string()).collect();
        let out = filter_to_active(&tools, &mcp_names, &promoted);
        let names: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["read", "review", "mcp__search"]);
    }

    #[test]
    fn filter_hides_unpromoted_mcp_only() {
        let tools = json!([def("read"), def("review"), def("mcp__search")]);
        let mcp_names: Vec<String> = ["mcp__search"].iter().map(|s| s.to_string()).collect();
        let promoted = HashSet::new();
        let out = filter_to_active(&tools, &mcp_names, &promoted);
        let names: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["read", "review"]);
    }

    #[test]
    fn filter_always_keeps_list_tools() {
        let tools = json!([def("list_tools"), def("mcp__search")]);
        let mcp_names: Vec<String> = ["mcp__search"].iter().map(|s| s.to_string()).collect();
        let promoted = HashSet::new();
        let out = filter_to_active(&tools, &mcp_names, &promoted);
        let names: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_tools"]);
    }

    #[test]
    fn promote_is_idempotent_and_visible() {
        let p = PromotedTools::new();
        assert!(!p.contains("mcp__search"));
        p.promote("mcp__search");
        assert!(p.contains("mcp__search"));
        p.promote("mcp__search");
        assert_eq!(p.snapshot().len(), 1);
    }

    fn spec_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn tool_names(val: &Value) -> Vec<String> {
        val.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect()
    }

    fn empty_build() -> ToolBuild {
        ToolBuild {
            vars: Vars::new(),
            excluded: Vec::new(),
            mcp: None,
        }
    }

    fn config_with_dynamic(enabled: bool) -> AgentConfig {
        let mut c = AgentConfig::default();
        c.dynamic_tools.enabled = enabled;
        c.dynamic_tools.core = Vec::new();
        c
    }

    #[test]
    fn disabled_advertises_everything() {
        let cfg = config_with_dynamic(false);
        let dynamic = DynamicContext::from_config(&cfg);
        let build = empty_build();
        let tools =
            build_active_tools(&build, &spec_model(), &cfg, &dynamic, &PromotedTools::new());
        let names = tool_names(&tools);
        assert!(names.contains(&"review".to_string()));
        assert!(names.contains(&"list_tools".to_string()));
    }

    #[test]
    fn enabled_keeps_all_builtins_without_promotion() {
        let cfg = config_with_dynamic(true);
        let dynamic = DynamicContext::from_config(&cfg);
        let build = empty_build();
        let promoted = PromotedTools::new();

        let tools = build_active_tools(&build, &spec_model(), &cfg, &dynamic, &promoted);
        let names = tool_names(&tools);
        assert!(names.contains(&"read".to_string()));
        assert!(
            names.contains(&"review".to_string()),
            "review is a builtin and should always be advertised"
        );
        assert!(names.contains(&"list_tools".to_string()));
    }

    #[test]
    fn enabled_advertises_builtins_and_list_tools() {
        let cfg = config_with_dynamic(true);
        let dynamic = DynamicContext::from_config(&cfg);
        let build = empty_build();
        let tools =
            build_active_tools(&build, &spec_model(), &cfg, &dynamic, &PromotedTools::new());
        assert!(tool_names(&tools).contains(&"list_tools".to_string()));
        assert!(tool_names(&tools).contains(&"review".to_string()));
    }
}

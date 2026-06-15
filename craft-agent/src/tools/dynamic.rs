//! Static-tier dynamic tool discovery. A small **core** set of tools is advertised
//! every turn; **extended** tools (and all MCP tools) are only advertised after the
//! model discovers and promotes them via the `list_tools` tool. The active set
//! advertised to the provider is `core ∪ promoted`, rebuilt each turn so a
//! mid-session promotion shows up on the very next request.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use craft_providers::Model;

use crate::mcp::McpHandle;
use crate::template::Vars;
use crate::{AgentConfig, tools};

use super::registry::{ToolRegistry, ToolTier};
use super::{DescriptionContext, ToolFilter};

/// Lua/builtin plugins that ship in the core tier. Native core tools declare their
/// tier via `impl_tool!(..., tier = ToolTier::Core)`; plugins go through the Lua
/// `Tool` impl which has no tier metadata, so they are listed here instead.
pub const DEFAULT_CORE_PLUGINS: &[&str] = &["bash", "glob", "index"];

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

/// Resolved dynamic-discovery state for a session: whether it is on, and the core
/// tool-name set (either the configured override or the computed default).
#[derive(Clone)]
pub struct DynamicContext {
    pub enabled: bool,
    pub core: Arc<HashSet<String>>,
}

impl DynamicContext {
    /// `enabled = false` advertises the full toolset (current pre-discovery behavior).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            core: Arc::new(HashSet::new()),
        }
    }

    pub fn from_config(config: &AgentConfig) -> Self {
        let dt = &config.dynamic_tools;
        if !dt.enabled {
            return Self::disabled();
        }
        let core = if dt.core.is_empty() {
            default_core_tools()
        } else {
            dt.core.iter().cloned().collect()
        };
        Self {
            enabled: true,
            core: Arc::new(core),
        }
    }
}

/// Native core tools (those declaring `tier = Core`) plus the high-frequency plugins.
pub fn default_core_tools() -> HashSet<String> {
    let mut core: HashSet<String> = ToolRegistry::native()
        .iter()
        .iter()
        .filter(|e| e.tool.tier() == ToolTier::Core)
        .map(|e| e.name().to_string())
        .collect();
    for &plugin in DEFAULT_CORE_PLUGINS {
        core.insert(plugin.to_string());
    }
    core
}

/// Everything the agent needs to rebuild the advertised tool list each turn.
#[derive(Clone)]
pub struct ToolBuild {
    pub vars: Vars,
    pub excluded: Vec<&'static str>,
    pub mcp: Option<McpHandle>,
}

/// Build the tool definitions sent to the provider this turn. When dynamic discovery
/// is on, the result is filtered to the active set (`core ∪ promoted`); otherwise the
/// full toolset is returned unchanged.
pub fn build_active_tools(
    build: &ToolBuild,
    model: &Model,
    config: &AgentConfig,
    dynamic: &DynamicContext,
    promoted: &PromotedTools,
) -> Value {
    let filter = ToolFilter::from_config(config, &build.excluded);
    let ctx = DescriptionContext { filter: &filter };
    let mut tools = ToolRegistry::native().definitions(&build.vars, &ctx, model.supports_tool_examples());
    if let Some(handle) = &build.mcp {
        handle.extend_tools(&mut tools);
    }
    if dynamic.enabled {
        let promoted_snap = promoted.snapshot();
        tools = filter_to_active(&tools, &dynamic.core, &promoted_snap);
    }
    tools
}

/// Keep a tool definition iff it is core, already promoted, or the discovery tool
/// itself. Definitions without a parseable `name` are kept defensively.
pub fn filter_to_active(tools: &Value, core: &HashSet<String>, promoted: &HashSet<String>) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    let kept: Vec<Value> = arr
        .iter()
        .filter(|def| {
            let Some(name) = def.get("name").and_then(|v| v.as_str()) else {
                return true;
            };
            name == tools::LIST_TOOLS_TOOL_NAME
                || core.contains(name)
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
    fn filter_keeps_core_and_promoted() {
        let tools = json!([def("read"), def("review"), def("mcp__search")]);
        let core: HashSet<String> = ["read"].iter().map(|s| s.to_string()).collect();
        let promoted: HashSet<String> = ["mcp__search"].iter().map(|s| s.to_string()).collect();
        let out = filter_to_active(&tools, &core, &promoted);
        let names: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["read", "mcp__search"]);
    }

    #[test]
    fn filter_always_keeps_list_tools() {
        let tools = json!([def("list_tools"), def("review")]);
        let core = HashSet::new();
        let promoted = HashSet::new();
        let out = filter_to_active(&tools, &core, &promoted);
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
        assert!(!p.contains("review"));
        p.promote("review");
        assert!(p.contains("review"));
        p.promote("review");
        assert_eq!(p.snapshot().len(), 1);
    }

    #[test]
    fn default_core_includes_native_core_and_plugins() {
        let core = default_core_tools();
        assert!(core.contains("read"));
        assert!(core.contains("bash"));
        assert!(!core.contains("review"));
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
    fn disabled_advertises_extended_tools() {
        let cfg = config_with_dynamic(false);
        let dynamic = DynamicContext::from_config(&cfg);
        let build = empty_build();
        let tools = build_active_tools(&build, &spec_model(), &cfg, &dynamic, &PromotedTools::new());
        let names = tool_names(&tools);
        assert!(names.contains(&"review".to_string()));
    }

    #[test]
    fn enabled_hides_extended_until_promoted() {
        let cfg = config_with_dynamic(true);
        let dynamic = DynamicContext::from_config(&cfg);
        let build = empty_build();
        let promoted = PromotedTools::new();

        let tools = build_active_tools(&build, &spec_model(), &cfg, &dynamic, &promoted);
        let names = tool_names(&tools);
        assert!(names.contains(&"read".to_string()));
        assert!(
            !names.contains(&"review".to_string()),
            "review should be hidden until promoted"
        );

        promoted.promote("review");
        let tools = build_active_tools(&build, &spec_model(), &cfg, &dynamic, &promoted);
        assert!(
            tool_names(&tools).contains(&"review".to_string()),
            "review advertised after promotion"
        );
    }

    #[test]
    fn enabled_reduces_advertised_schema_size() {
        let build = empty_build();
        let promoted = PromotedTools::new();

        let cfg_on = config_with_dynamic(true);
        let dyn_on = DynamicContext::from_config(&cfg_on);
        let active = build_active_tools(&build, &spec_model(), &cfg_on, &dyn_on, &promoted);

        let cfg_off = config_with_dynamic(false);
        let dyn_off = DynamicContext::from_config(&cfg_off);
        let full = build_active_tools(&build, &spec_model(), &cfg_off, &dyn_off, &promoted);

        assert!(tool_names(&active).len() < tool_names(&full).len());
        assert!(
            active.to_string().len() < full.to_string().len(),
            "active schema payload must be smaller than the full set"
        );
    }

    #[test]
    fn list_tools_always_advertised_when_enabled() {
        let cfg = config_with_dynamic(true);
        let dynamic = DynamicContext::from_config(&cfg);
        let build = empty_build();
        let tools = build_active_tools(&build, &spec_model(), &cfg, &dynamic, &PromotedTools::new());
        assert!(tool_names(&tools).contains(&"list_tools".to_string()));
    }
}

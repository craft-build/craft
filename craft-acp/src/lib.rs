pub mod commands;
pub mod mcp;
pub mod methods;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use craft_agent::mcp::config::McpConfig;
use craft_agent::permissions::PluginRuleStore;
use craft_agent::prompt::ResolvedSlots;
use craft_agent::{AgentConfig, PermissionsConfig};
use craft_config::ModelPolicy;
use craft_lua::PluginHost;
use craft_providers::Timeouts;
use craft_providers::model::Model;
use craft_storage::flow::FlowStore;

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub mcp_config: McpConfig,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub yolo: bool,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_host: PluginHost,
    pub flow_store: Arc<FlowStore>,
    pub plugin_rules: Arc<PluginRuleStore>,
}

pub async fn run(params: AcpParams) -> color_eyre::Result<()> {
    server::serve(params).await
}

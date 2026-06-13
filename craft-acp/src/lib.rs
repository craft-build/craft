pub mod methods;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use craft_agent::prompt::ResolvedSlots;
use craft_agent::{AgentConfig, PermissionsConfig};
use craft_providers::Timeouts;
use craft_providers::model::Model;

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub mcp_handle: Option<craft_agent::McpHandle>,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub yolo: bool,
}

pub async fn run(params: AcpParams) -> color_eyre::Result<()> {
    server::serve(params).await
}

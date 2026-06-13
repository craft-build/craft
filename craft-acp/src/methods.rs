use agent_client_protocol_schema::{
    AgentCapabilities, Implementation, InitializeResponse, LoadSessionResponse, McpCapabilities,
    NewSessionResponse, PromptCapabilities, ProtocolVersion, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";

pub const MODEL_CONFIG_ID: &str = "model";
pub const MODE_CONFIG_ID: &str = "mode";

pub fn initialize_response() -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(McpCapabilities::default()),
        )
        .auth_methods(vec![])
        .agent_info(Implementation::new("craft", VERSION))
}

pub fn mode_config_option(current: &str) -> SessionConfigOption {
    let options = vec![
        SessionConfigSelectOption::new(MODE_BUILD.to_string(), "Build"),
        SessionConfigSelectOption::new(MODE_PLAN.to_string(), "Plan"),
    ];
    SessionConfigOption::select(MODE_CONFIG_ID, "Mode", current.to_string(), options)
        .category(SessionConfigOptionCategory::Mode)
}

fn model_config_option_default() -> SessionConfigOption {
    SessionConfigOption::select(
        MODEL_CONFIG_ID,
        "Model",
        "",
        Vec::<SessionConfigSelectOption>::new(),
    )
    .category(SessionConfigOptionCategory::Model)
}

pub fn new_session_response(session_id: &str) -> NewSessionResponse {
    NewSessionResponse::new(session_id.to_string())
        .config_options(vec![mode_config_option(MODE_BUILD), model_config_option_default()])
}

pub fn load_session_response() -> LoadSessionResponse {
    LoadSessionResponse::new()
        .config_options(vec![mode_config_option(MODE_BUILD), model_config_option_default()])
}

pub fn model_config_option(current: &str, specs: &[String]) -> SessionConfigOption {
    let mut options: Vec<SessionConfigSelectOption> = specs
        .iter()
        .map(|spec| SessionConfigSelectOption::new(spec.clone(), spec.clone()))
        .collect();
    if !specs.iter().any(|spec| spec == current) {
        options.insert(
            0,
            SessionConfigSelectOption::new(current.to_string(), current.to_string()),
        );
    }
    SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.to_string(), options)
        .category(SessionConfigOptionCategory::Model)
}

pub fn mode_id_to_agent_mode(mode_id: &str) -> Option<craft_agent::AgentMode> {
    match mode_id {
        MODE_BUILD => Some(craft_agent::AgentMode::Build),
        MODE_PLAN => {
            let storage = craft_storage::StateDir::resolve().ok()?;
            let plan_path = craft_storage::plans::new_plan_path(&storage).ok()?;
            Some(craft_agent::AgentMode::Plan(plan_path))
        }
        _ => None,
    }
}

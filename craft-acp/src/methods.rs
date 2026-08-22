use agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::v1::{
    AgentCapabilities, Implementation, InitializeResponse, LoadSessionResponse, McpCapabilities,
    NewSessionResponse, PromptCapabilities, ResumeSessionResponse, SessionCapabilities,
    SessionCloseCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectOption, SessionListCapabilities, SessionMode, SessionModeId,
    SessionModeState, SessionResumeCapabilities,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";
pub const MODE_FLOW: &str = "flow";

pub const MODEL_CONFIG_ID: &str = "model";
pub const THINKING_CONFIG_ID: &str = "thinking";
pub const MODE_CONFIG_ID: &str = "mode";
pub const YOLO_CONFIG_ID: &str = "yolo";
pub const AUTO_REVIEW_CONFIG_ID: &str = "auto_review";

pub fn initialize_response() -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .session_capabilities(
                    SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .resume(SessionResumeCapabilities::new())
                        .close(SessionCloseCapabilities::new()),
                )
                .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
                .mcp_capabilities(McpCapabilities::new().http(true).sse(true)),
        )
        .auth_methods(vec![])
        .agent_info(Implementation::new("craft", VERSION))
}

pub fn mode_config_option(current: &str) -> SessionConfigOption {
    let options = vec![
        SessionConfigSelectOption::new(MODE_BUILD.to_string(), "Build"),
        SessionConfigSelectOption::new(MODE_PLAN.to_string(), "Plan"),
        SessionConfigSelectOption::new(MODE_FLOW.to_string(), "Flow"),
    ];
    SessionConfigOption::select(MODE_CONFIG_ID, "Mode", current.to_string(), options)
        .category(SessionConfigOptionCategory::Mode)
}

pub fn yolo_config_option(current: bool) -> SessionConfigOption {
    SessionConfigOption::select(
        YOLO_CONFIG_ID,
        "Yolo",
        current.to_string(),
        vec![
            SessionConfigSelectOption::new("false", "Off"),
            SessionConfigSelectOption::new("true", "On"),
        ],
    )
    .description("Auto-approve all tool permissions")
}

pub fn auto_review_config_option(current: bool) -> SessionConfigOption {
    SessionConfigOption::select(
        AUTO_REVIEW_CONFIG_ID,
        "Auto Review",
        current.to_string(),
        vec![
            SessionConfigSelectOption::new("false", "Off"),
            SessionConfigSelectOption::new("true", "On"),
        ],
    )
    .description("Let an LLM reviewer auto-allow or deny non-approved tool calls")
}

pub const THINKING_LEVELS: [(&str, &str); 5] = [
    ("off", "Off"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "XHigh"),
];

pub fn thinking_config_option(current: &str) -> SessionConfigOption {
    let options: Vec<_> = THINKING_LEVELS
        .iter()
        .map(|(value, name)| SessionConfigSelectOption::new(value.to_string(), name.to_string()))
        .collect();
    SessionConfigOption::select(THINKING_CONFIG_ID, "Thinking", current.to_string(), options)
        .category(SessionConfigOptionCategory::Model)
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

fn session_modes(current: &str) -> SessionModeState {
    SessionModeState::new(
        SessionModeId::from(current.to_string()),
        vec![
            SessionMode::new(MODE_BUILD.to_string(), "Build"),
            SessionMode::new(MODE_PLAN.to_string(), "Plan"),
            SessionMode::new(MODE_FLOW.to_string(), "Flow"),
        ],
    )
}

pub fn new_session_response(session_id: &str) -> NewSessionResponse {
    NewSessionResponse::new(session_id.to_string())
        .modes(session_modes(MODE_BUILD))
        .config_options(vec![
            mode_config_option(MODE_BUILD),
            model_config_option_default(),
            thinking_config_option("off"),
        ])
}

pub fn load_session_response() -> LoadSessionResponse {
    LoadSessionResponse::new()
        .modes(session_modes(MODE_BUILD))
        .config_options(vec![
            mode_config_option(MODE_BUILD),
            model_config_option_default(),
            thinking_config_option("off"),
        ])
}

pub fn resume_session_response() -> ResumeSessionResponse {
    ResumeSessionResponse::new()
        .modes(session_modes(MODE_BUILD))
        .config_options(vec![
            mode_config_option(MODE_BUILD),
            model_config_option_default(),
            thinking_config_option("off"),
        ])
}

pub fn model_config_option(current: &str, specs: &[String]) -> SessionConfigOption {
    // Duplicated specs (e.g. a model reachable through two provider configs)
    // must not leak into the option list: clients key menu items by value.
    let mut seen = std::collections::HashSet::new();
    let mut options: Vec<SessionConfigSelectOption> = specs
        .iter()
        .filter(|spec| seen.insert(spec.as_str()))
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
        MODE_FLOW => Some(craft_agent::AgentMode::Flow(new_workstream_id())),
        _ => None,
    }
}

/// Fresh opaque workstream id (16 hex chars from 8 random bytes), matching the
/// convention `craft-ui`'s Flow mode uses (see `craft-ui/src/app/mode.rs`).
fn new_workstream_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

use std::path::PathBuf;
use std::sync::Arc;

use craft_providers::Message;
use craft_providers::StopReason;
use craft_providers::Timeouts;
use craft_providers::TokenUsage;
use craft_providers::model::Model;
use craft_providers::provider::{self, Provider};
use craft_storage::StateDir;
use craft_storage::id::{CraftId, SessionRef};
use craft_storage::sessions::Session;
use flume::Receiver;
use serde_json::Value;
use tracing::{error, warn};

use crate::agent::{self, History};
use crate::cancel::{CancelMap, CancelToken};
use crate::permissions::PermissionManager;
use crate::prompt::ResolvedSlots;
use crate::template;
use crate::tools::{FileReadTracker, FsBackend, ToolRegistry};
use crate::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams,
    Envelope, EventSender, ImageSource, McpHandle, PermissionsConfig, ToolOutput, ToolOutputLines,
};

type StoredSession = Session<Message, TokenUsage, ToolOutput>;

fn error_event(error: AgentError) -> AgentEvent {
    match error {
        AgentError::Cancelled => AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 0,
            stop_reason: Some(StopReason::Cancelled),
        },
        e => {
            error!(error = %e, "agent error");
            AgentEvent::Error {
                message: e.user_message(),
            }
        }
    }
}

struct SessionStore {
    dir: StateDir,
    session: StoredSession,
}

impl SessionStore {
    fn open(session_id: CraftId, cwd: &str, model_spec: &str) -> Option<Self> {
        let dir = StateDir::resolve()
            .map_err(|e| warn!(error = %e, "state dir unavailable; session will not be persisted"))
            .ok()?;
        Some(Self::open_in(dir, session_id, cwd, model_spec))
    }

    fn open_in(dir: StateDir, session_id: CraftId, cwd: &str, model_spec: &str) -> Self {
        match StoredSession::load(session_id, &dir) {
            Ok(session) => Self { dir, session },
            Err(_) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = SessionRef::from_id(session_id);
                let mut store = Self { dir, session };
                store.save();
                store
            }
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.session.save(&self.dir) {
            warn!(error = %e, session_id = %self.session.id, "failed to persist session");
        }
    }

    fn record_turn(&mut self, messages: &[Message], model_spec: String) {
        self.session.messages = messages.to_vec();
        self.session.model = model_spec;
        self.session.update_title_if_default();
        self.save();
    }
}

pub struct HeadlessParams {
    pub model: Model,
    pub config: AgentConfig,
    pub compression: craft_config::CompressionConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt: String,
    pub images: Vec<ImageSource>,
    pub prompt_slots: ResolvedSlots,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub fast: bool,
}

pub struct HeadlessHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub session_id: SessionRef,
    pub cwd: String,
    pub task: tokio::task::JoinHandle<()>,
}

struct AgentSetup {
    vars: template::Vars,
    instructions: agent::Instructions,
    tools: Value,
    tool_build: crate::tools::ToolBuild,
    promoted: crate::tools::PromotedTools,
}

fn setup(
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    mcp_handle: Option<&McpHandle>,
) -> AgentSetup {
    let vars = template::env_vars();
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));
    let tool_build = crate::tools::ToolBuild {
        vars: vars.clone(),
        excluded: excluded_tools.to_vec(),
        mcp: mcp_handle.cloned(),
    };
    let dynamic = crate::tools::DynamicContext::from_config(config);
    let promoted = crate::tools::PromotedTools::new();
    let tools = crate::tools::build_active_tools(&tool_build, model, config, &dynamic, &promoted);
    AgentSetup {
        vars,
        instructions,
        tools,
        tool_build,
        promoted,
    }
}

pub fn spawn(params: HeadlessParams) -> HeadlessHandle {
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mode = AgentMode::Build;
    let AgentSetup {
        vars,
        instructions,
        tools,
        tool_build,
        promoted,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.mcp_handle.as_ref(),
    );

    let compact = params
        .config
        .small_model
        .should_activate(params.model.context_window)
        && params.config.small_model.compact_prompt;
    let system = agent::build_system_prompt(
        &vars,
        &mode,
        &instructions.text,
        &params.prompt_slots,
        &params.model,
        compact,
    );

    let tool_names = extract_tool_names(&tools);

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();

    let session_id = CraftId::generate();
    let session_ref = SessionRef::from(session_id);

    let task = tokio::spawn({
        let session_ref_clone = session_ref.clone();
        let mcp_shutdown = params.mcp_handle.clone();
        let working_dir_path = params.initial_wd.clone();
        async move {
            let event_tx = EventSender::new(raw_tx, 0);
            let mut model = params.model;
            let provider: Arc<dyn Provider> =
                match provider::from_model(&mut model, params.timeouts).await {
                    Ok(p) => Arc::from(p),
                    Err(e) => {
                        error!(error = %e, "provider error");
                        let _ = event_tx.send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        return;
                    }
                };
            let error_tx = event_tx.clone();
            let mut history = History::new(Vec::new());
            let agent = Agent::new(
                AgentParams {
                    provider,
                    model,
                    config: params.config,
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::new(PermissionManager::new(
                        params.permissions_config,
                        working_dir_path,
                    )),
                    session_id: Some(session_ref_clone.clone()),
                    timeouts: params.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::new(params.prompt_slots),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    registry: Arc::clone(ToolRegistry::native_arc()),
                    compression: params.compression,
                    findings_store: Some(crate::FindingsStore::new_shared()),
                    fs: Arc::new(crate::tools::LocalFs),
                    doom: Arc::new(std::sync::Mutex::new(crate::DoomTracker::new())),
                },
                AgentRunParams {
                    history: &mut history,
                    system,
                    event_tx,
                    tools,
                    promoted,
                    tool_build: Some(tool_build),
                    hooks: None,
                },
            )
            .with_loaded_instructions(instructions.loaded)
            .with_mcp(params.mcp_handle);

            let result = agent
                .run(AgentInput {
                    message: params.prompt,
                    mode,
                    images: params.images,
                    fast: params.fast,
                    ..Default::default()
                })
                .await;

            if let Err(e) = result {
                let _ = error_tx.send(error_event(e));
            }

            if let Some(handle) = mcp_shutdown {
                handle.shutdown().await;
            }
        }
    });

    HeadlessHandle {
        event_rx,
        tool_names,
        session_id: session_ref,
        cwd: working_dir,
        task,
    }
}

pub struct InteractiveParams {
    pub model: Model,
    pub config: AgentConfig,
    pub compression: craft_config::CompressionConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub session_id: Option<SessionRef>,
    pub initial_history: Vec<Message>,
    pub yolo: bool,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    pub fs: Arc<dyn FsBackend>,
}

pub struct InteractiveHandle {
    pub event_rx: Receiver<Envelope>,
    /// Sender half of `event_rx`'s channel. Exposed so callers that drive
    /// out-of-band work against this session (for example craft-acp's Flow
    /// pipeline, which craft-agent cannot depend on without a dependency
    /// cycle) can inject `Envelope`s that flow through the same translation
    /// path as regular agent turns instead of building a parallel one.
    pub raw_event_tx: flume::Sender<Envelope>,
    pub tool_names: Vec<String>,
    pub input_tx: flume::Sender<AgentInput>,
    pub answer_tx: flume::Sender<String>,
    pub cancel_tx: flume::Sender<()>,
    pub model_tx: flume::Sender<Model>,
    pub session_id: SessionRef,
    pub permissions: Arc<PermissionManager>,
    pub task: tokio::task::JoinHandle<()>,
}

pub fn spawn_interactive(params: InteractiveParams) -> InteractiveHandle {
    let AgentSetup {
        vars,
        instructions,
        mut tools,
        tool_build,
        promoted,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.mcp_handle.as_ref(),
    );

    let tool_names = extract_tool_names(&tools);

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let raw_event_tx = raw_tx.clone();
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();

    let (session_id, session_ref) = match params.session_id.clone() {
        Some(w) => (w.id(), w),
        None => {
            let id = CraftId::generate();
            (id, SessionRef::from(id))
        }
    };

    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let permissions = Arc::new(PermissionManager::new(
        params.permissions_config,
        params.initial_wd.clone(),
    ));
    if params.yolo {
        permissions.toggle_yolo();
    }

    let answer_rx = Arc::new(tokio::sync::Mutex::new(answer_rx));
    let file_tracker = FileReadTracker::fresh();

    let task = tokio::spawn({
        let session_ref_clone = session_ref.clone();
        let permissions = Arc::clone(&permissions);
        async move {
            let mut model = params.model;
            let mut provider: Arc<dyn Provider> =
                match provider::from_model(&mut model, params.timeouts).await {
                    Ok(p) => Arc::from(p),
                    Err(e) => {
                        error!(error = %e, "provider error");
                        let _ = EventSender::new(raw_tx, 0).send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        return;
                    }
                };

            let mut store = SessionStore::open(session_id, &working_dir, &model.spec());
            let mut history = History::restored(params.initial_history);
            let mut run_id: u64 = 0;

            while let Ok(input) = input_rx.recv_async().await {
                let event_tx = EventSender::new(raw_tx.clone(), run_id);
                let error_tx = event_tx.clone();

                if let Some(mut new_model) = model_rx.try_iter().last()
                    && new_model.spec() != model.spec()
                {
                    match provider::from_model(&mut new_model, params.timeouts).await {
                        Ok(p) => {
                            provider = Arc::from(p);
                            let dynamic = crate::tools::DynamicContext::from_config(&params.config);
                            tools = crate::tools::build_active_tools(
                                &tool_build,
                                &new_model,
                                &params.config,
                                &dynamic,
                                &promoted,
                            );
                            model = new_model;
                        }
                        Err(e) => {
                            error!(error = %e, "provider error");
                            let _ = error_tx.send(AgentEvent::Error {
                                message: e.user_message(),
                            });
                            run_id += 1;
                            continue;
                        }
                    }
                }

                let compact = params
                    .config
                    .small_model
                    .should_activate(model.context_window)
                    && params.config.small_model.compact_prompt;
                let mut system = params.system_prompt_override.clone().unwrap_or_else(|| {
                    agent::build_system_prompt(
                        &vars,
                        &input.mode,
                        &instructions.text,
                        &params.prompt_slots,
                        &model,
                        compact,
                    )
                });
                if let Some(append) = &params.append_system_prompt {
                    system.push('\n');
                    system.push_str(append);
                }

                let (trigger, cancel) = CancelToken::new();
                let cancel_task = tokio::spawn({
                    let cancel_rx = cancel_rx.clone();
                    async move {
                        if cancel_rx.recv_async().await.is_ok() {
                            trigger.cancel();
                        }
                    }
                });

                while answer_rx.lock().await.try_recv().is_ok() {}

                let agent = Agent::new(
                    AgentParams {
                        provider: Arc::clone(&provider),
                        model: model.clone(),
                        config: params.config.clone(),
                        tool_output_lines: ToolOutputLines::default(),
                        permissions: Arc::clone(&permissions),
                        session_id: Some(session_ref_clone.clone()),
                        timeouts: params.timeouts,
                        file_tracker: Arc::clone(&file_tracker),
                        prompt_slots: Arc::clone(&params.prompt_slots),
                        subagent_cancels: Arc::new(CancelMap::new()),
                        registry: Arc::clone(ToolRegistry::native_arc()),
                        compression: params.compression.clone(),
                        findings_store: Some(crate::FindingsStore::new_shared()),
                        fs: Arc::clone(&params.fs),
                        doom: Arc::new(std::sync::Mutex::new(crate::DoomTracker::new())),
                    },
                    AgentRunParams {
                        history: &mut history,
                        system,
                        event_tx,
                        tools: tools.clone(),
                        promoted: promoted.clone(),
                        tool_build: Some(tool_build.clone()),
                        hooks: None,
                    },
                )
                .with_loaded_instructions(instructions.loaded.clone())
                .with_user_response_rx(Arc::clone(&answer_rx))
                .with_host_question_routing(true)
                .with_cancel(cancel)
                .with_mcp(params.mcp_handle.clone());

                let agent = {
                    let role = craft_providers::roles::resolve_role(
                        craft_config::model_roles::ModelRole::Default,
                        model.clone(),
                        Arc::clone(&provider),
                        params.timeouts,
                    )
                    .await;
                    agent.with_fallback_chain(role.fallbacks)
                };

                let result = agent.run(input).await;
                cancel_task.abort();

                if let Err(e) = result {
                    let _ = error_tx.send(error_event(e));
                }

                if let Some(store) = &mut store {
                    store.record_turn(history.as_slice(), model.spec());
                }
                run_id += 1;
            }

            if let Some(handle) = params.mcp_handle {
                handle.shutdown().await;
            }
        }
    });

    InteractiveHandle {
        event_rx,
        raw_event_tx,
        tool_names,
        input_tx,
        answer_tx,
        cancel_tx,
        model_tx,
        session_id: session_ref,
        permissions,
        task,
    }
}

fn extract_tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use craft_storage::sessions::generate_title;
    use tempfile::TempDir;

    use super::*;

    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";

    fn session_id() -> CraftId {
        SESSION_ID.parse().unwrap()
    }

    fn store_in(tmp: &TempDir) -> SessionStore {
        SessionStore::open_in(
            StateDir::from_path(tmp.path().to_path_buf()),
            session_id(),
            CWD,
            MODEL_SPEC,
        )
    }

    fn load(tmp: &TempDir) -> StoredSession {
        StoredSession::load(session_id(), &StateDir::from_path(tmp.path().to_path_buf())).unwrap()
    }

    #[test]
    fn new_session_is_loadable_before_first_turn() {
        let tmp = TempDir::new().unwrap();
        store_in(&tmp);
        let loaded = load(&tmp);
        assert_eq!(loaded.id.id(), session_id());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert!(loaded.messages.is_empty());
    }

    #[test]
    fn record_turn_persists_messages_and_title() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let messages = vec![Message::user("fix the login bug".into())];
        store.record_turn(&messages, MODEL_SPEC.into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.title, generate_title(&messages));
    }

    #[test]
    fn reopening_resumes_existing_session() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(&[Message::user("first prompt".into())], MODEL_SPEC.into());
        drop(store);

        let mut store = store_in(&tmp);
        assert_eq!(store.session.messages.len(), 1);

        let messages = vec![
            Message::user("first prompt".into()),
            Message::user("second prompt".into()),
        ];
        store.record_turn(&messages, "other/model".into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }
}

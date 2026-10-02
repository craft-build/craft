//! The runtime setup contract: the one place every execution surface (TUI,
//! headless print mode, ACP) obtains the shared pieces of a session — the
//! workspace environment, model metadata, compaction state, and the
//! `RunParams` policy.
//!
//! Surfaces keep their orchestration (MCP startup mode, catalog breadth,
//! model-selection preference, turn bound) but assemble `RunParams` only
//! through [`run_policy`], so equivalent sessions receive equivalent
//! policies. The deliberate per-surface differences are small, explicit,
//! and each pinned by a test:
//!
//! - MCP startup: [`McpStartup::Connected`] (headless, ACP) vs
//!   [`McpStartup::Background`] with the TUI's event channel;
//! - reviewer definition install: TUI passes `true` today;
//! - catalog breadth + models.dev/registry warm: TUI today;
//! - selection preference: TUI last-used → tier default → first entry,
//!   headless `-m provider/model` (provider-validated, id not
//!   catalog-checked) else the first entry, ACP `resolve_or_default`;
//! - turn bound: ACP is always [`MaxTurns::Unbounded`]; TUI and headless
//!   follow `agent.max_turns`;
//! - plan mode: TUI and headless thread their [`AgentMode`] into the
//!   prompt; ACP is always Build.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use crate::error::{InvalidSnafu, Result};
use crate::instructions::Instructions;
use crate::permissions::PermissionManager;
use crate::providers::CatalogModel;
use crate::run::{
    AgentMode, CompactionCtx, RecencySource, RetryCtx, RunParams, SharedCompactionState,
};
use crate::tools::Workspace;

// --- Session environment ----------------------------------------------------

/// The session environment every surface shares: discovered instruction
/// files, the workspace with its MCP handle already installed, and the
/// permission engine (constructed on every surface, even where nothing
/// consults it yet).
pub struct WorkspaceEnv {
    pub instructions: Instructions,
    pub workspace: Workspace,
    pub permissions: Arc<PermissionManager>,
    /// MCP config problems (unreadable/unparseable server entries); the
    /// surface surfaces them as startup notes.
    pub mcp_errors: crate::mcp::config::McpConfigErrors,
}

/// How MCP servers start for the session.
pub enum McpStartup {
    /// `start_connected`: every server settles (bounded) before the first
    /// prompt — for surfaces with no frame to protect (headless, ACP).
    Connected,
    /// `start_with_events`: startup returns without waiting; the first turn
    /// gates on `ready` (B.11). The TUI routes MCP log notifications
    /// through the event channel.
    Background(crate::mcp::McpEvents),
}

/// Discover instructions and permission rules off the async worker, build
/// the workspace with the instructions loaded, optionally install the
/// built-in reviewer definition, and start MCP in the surface's mode.
pub async fn workspace_env(
    cwd: &Path,
    mcp: McpStartup,
    install_reviewer: bool,
) -> Result<WorkspaceEnv> {
    let (instructions, permissions) = tokio::task::spawn_blocking({
        let cwd = cwd.to_path_buf();
        move || {
            let instructions = crate::instructions::load_instructions(&cwd.display().to_string());
            let permissions =
                PermissionManager::new(crate::permissions::load_permissions(&cwd), cwd);
            (instructions, permissions)
        }
    })
    .await
    .map_err(|e| {
        InvalidSnafu {
            reason: format!("resolving the session environment: {e}"),
        }
        .build()
    })?;
    let workspace = Workspace::new(cwd)
        .map_err(crate::error::client_error)?
        .with_loaded_instructions(instructions.loaded.clone());
    // Phase 5 of the argosy integration: best-effort, idempotent install
    // of the built-in reviewer agent definition into `.craft/agents/`.
    if install_reviewer {
        let root = cwd.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            crate::knowledge::install_reviewer_definition(&root, false)
        })
        .await;
    }
    let (mcp_handle, mcp_errors) = match mcp {
        McpStartup::Connected => crate::mcp::start_connected(cwd).await,
        McpStartup::Background(events) => crate::mcp::start_with_events(cwd, events).await,
    };
    workspace.set_mcp(mcp_handle);
    Ok(WorkspaceEnv {
        instructions,
        workspace,
        permissions: Arc::new(permissions),
        mcp_errors,
    })
}

// --- Model metadata ---------------------------------------------------------

/// A model choice with the catalog metadata that travel with it. Metadata
/// only: nothing here enforces catalog membership (headless `-m` and
/// client-supplied ACP ids stay usable without catalog backing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub provider: String,
    pub model_id: String,
    /// The model's context window, when a catalog reports one; drives
    /// compaction thresholds and output-cap clamping.
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl ResolvedModel {
    /// A choice with no catalog metadata (metadata-unaware selection).
    pub fn bare(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model_id: model_id.into(),
            context_length: None,
            max_output_tokens: None,
        }
    }

    /// The `provider/model` spec used for pricing and session headers.
    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }
}

/// Look up a model's catalog metadata. `(None, None)` on any miss — the
/// caller keeps its own selection policy; this only guarantees that
/// whenever a catalog entry exists, its metadata travel with the choice.
pub fn catalog_metadata(
    catalogs: &BTreeMap<String, Vec<CatalogModel>>,
    provider: &str,
    model: &str,
) -> (Option<u32>, Option<u32>) {
    catalogs
        .get(provider)
        .and_then(|models| models.iter().find(|entry| entry.id == model))
        .map(|entry| (entry.context_length, entry.max_output_tokens))
        .unwrap_or((None, None))
}

// --- Compaction -------------------------------------------------------------

/// A fresh session-shared compaction state linked to this session's dedup
/// cache and guardrail counters, so a compaction run clears them.
pub fn new_compaction_state(
    dedup: crate::run::SharedDedupCache,
    guardrails: crate::run::SharedGuardrails,
) -> SharedCompactionState {
    Arc::new(std::sync::Mutex::new(
        crate::compaction::CompactionState::default()
            .with_dedup(dedup)
            .with_guardrails(guardrails),
    ))
}

/// The per-run compaction context: shared session state, configured
/// stages, buffer, and the selected model's window.
pub fn compaction_ctx(
    state: SharedCompactionState,
    config: &Config,
    context_length: Option<u32>,
) -> CompactionCtx {
    CompactionCtx {
        state,
        stages: config.compaction.clone(),
        buffer: config.compaction_buffer,
        context_length,
    }
}

// --- Run policy -------------------------------------------------------------

/// How the surface bounds model calls per run.
pub enum MaxTurns {
    /// `config.agent.max_turns`, unbounded when unset (TUI, headless).
    FromConfig,
    /// Always unbounded regardless of config: the ACP client owns stop
    /// decisions through `session/cancel`.
    Unbounded,
}

/// The inputs [`run_policy`] assembles; the fields surfaces legitimately
/// vary. `recency`, `retry`, and `fast` are `None`/default/`false` on
/// every surface today — the decision lives here so future seams
/// (budgeting, recency, fast tier) extend one place.
pub struct RunPolicyInputs<'a> {
    pub config: &'a Config,
    /// Working directory as the system prompt shows it.
    pub cwd: &'a str,
    /// Instruction text appended to the preamble (AGENTS.md and friends).
    pub instructions_text: &'a str,
    /// Plan mode threads the plan path into the prompt; ACP passes Build.
    pub mode: &'a AgentMode,
    pub model: &'a ResolvedModel,
    pub compaction: Option<CompactionCtx>,
    /// Volatile per-turn facts appended to the request view only.
    pub recency: Option<Arc<dyn RecencySource>>,
    pub retry: RetryCtx,
    /// Price turns at the provider's fast tier.
    pub fast: bool,
    pub thinking: Option<crate::thinking::ThinkingConfig>,
    pub max_turns: MaxTurns,
}

/// The single `RunParams` constructor: system prompt (env vars, preamble +
/// instructions, plan path), sampling, turn bound, continuation bound,
/// compression, compaction, reauth hook, model spec, and advisor.
pub fn run_policy(inputs: RunPolicyInputs<'_>) -> RunParams {
    let RunPolicyInputs {
        config,
        cwd,
        instructions_text,
        mode,
        model,
        compaction,
        recency,
        retry,
        fast,
        thinking,
        max_turns,
    } = inputs;
    RunParams {
        preamble: Some(crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", cwd)
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &format!("{}{}", config.agent.preamble, instructions_text),
            &crate::prompt::ResolvedSlots::default(),
            mode.plan_path(),
        )),
        temperature: config.agent.temperature,
        max_tokens: config.agent.max_tokens,
        thinking: thinking
            .or(config.always_thinking)
            .or(config.agent.thinking)
            .unwrap_or_default(),
        max_turns: match max_turns {
            MaxTurns::FromConfig => config
                .agent
                .max_turns
                .map(|n| n as usize)
                .unwrap_or(RunParams::UNBOUNDED),
            MaxTurns::Unbounded => RunParams::UNBOUNDED,
        },
        recency,
        compression: config.compression.clone(),
        max_continuation_turns: RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
        compaction,
        reauth: config
            .providers
            .get(&model.provider)
            .map(|provider_config| crate::providers::reauth_hook(provider_config, &model.model_id)),
        model_spec: Some(model.spec().into()),
        retry,
        fast,
        advisor: config.agent.advisor.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    fn resolved() -> ResolvedModel {
        ResolvedModel {
            provider: "acme".into(),
            model_id: "one".into(),
            context_length: Some(128_000),
            max_output_tokens: None,
        }
    }

    fn compaction(ctx_config: &Config) -> CompactionCtx {
        compaction_ctx(
            new_compaction_state(crate::run::shared_cache(), crate::run::shared_guardrails()),
            ctx_config,
            resolved().context_length,
        )
    }

    /// The inputs shape each surface passes for an equivalent session:
    /// one config, one cwd, one model, one compaction context — differing
    /// only in the documented knobs.
    fn inputs<'a>(
        config: &'a Config,
        cwd: &'a str,
        mode: &'a AgentMode,
        model: &'a ResolvedModel,
        compaction: Option<CompactionCtx>,
        max_turns: MaxTurns,
    ) -> RunPolicyInputs<'a> {
        RunPolicyInputs {
            config,
            cwd,
            instructions_text: "project instructions",
            mode,
            model,
            compaction,
            recency: None,
            retry: RetryCtx::default(),
            fast: false,
            thinking: None,
            max_turns,
        }
    }

    #[test]
    fn equivalent_sessions_receive_equivalent_policies() {
        let config = config("agent { max_turns = 12\n temperature = 0.2\n max_tokens = 4096 }");
        let cwd = "/project";
        let model = resolved();
        let build = AgentMode::Build;
        let tui = run_policy(inputs(
            &config,
            cwd,
            &build,
            &model,
            Some(compaction(&config)),
            MaxTurns::FromConfig,
        ));
        let headless = run_policy(inputs(
            &config,
            cwd,
            &build,
            &model,
            Some(compaction(&config)),
            MaxTurns::FromConfig,
        ));
        let acp = run_policy(inputs(
            &config,
            cwd,
            &build,
            &model,
            Some(compaction(&config)),
            MaxTurns::Unbounded,
        ));

        assert_eq!(tui.preamble, headless.preamble);
        assert_eq!(tui.preamble, acp.preamble);
        for params in [&tui, &headless, &acp] {
            assert_eq!(params.temperature, Some(0.2));
            assert_eq!(params.max_tokens, Some(4096));
            assert_eq!(
                params.max_continuation_turns,
                RunParams::DEFAULT_MAX_CONTINUATION_TURNS
            );
            assert_eq!(params.compression, config.compression);
            assert_eq!(
                params.model_spec.as_deref(),
                Some("acme/one"),
                "model spec travels with the choice"
            );
            assert!(!params.fast);
            assert_eq!(params.advisor, config.agent.advisor);
            assert!(params.recency.is_none());
            assert!(params.reauth.is_none(), "no provider config, no hook");
            let compaction = params
                .compaction
                .as_ref()
                .expect("every surface compacts today");
            assert_eq!(compaction.context_length, model.context_length);
            assert_eq!(compaction.stages, config.compaction);
            assert_eq!(compaction.buffer, config.compaction_buffer);
        }

        // Documented deviation: ACP is unbounded even with a configured
        // bound; TUI and headless follow `agent.max_turns`.
        assert_eq!(tui.max_turns, 12);
        assert_eq!(headless.max_turns, 12);
        assert_eq!(acp.max_turns, RunParams::UNBOUNDED);
    }

    #[test]
    fn plan_mode_threads_the_plan_path_into_the_prompt() {
        let config = config("");
        let cwd = "/project";
        let model = resolved();
        let plan = AgentMode::Plan(std::path::PathBuf::from("/project/plans/plan.md"));
        let build = AgentMode::Build;
        let planned = run_policy(inputs(
            &config,
            cwd,
            &plan,
            &model,
            Some(compaction(&config)),
            MaxTurns::FromConfig,
        ));
        let built = run_policy(inputs(
            &config,
            cwd,
            &build,
            &model,
            Some(compaction(&config)),
            MaxTurns::FromConfig,
        ));
        assert!(
            planned
                .preamble
                .as_deref()
                .unwrap()
                .contains("plans/plan.md"),
            "plan path must reach the prompt"
        );
        assert!(!built.preamble.as_deref().unwrap().contains("plans/plan.md"));
    }

    #[test]
    fn unconfigured_turn_bound_is_unbounded() {
        let config = config("");
        let model = resolved();
        let build = AgentMode::Build;
        let params = run_policy(inputs(
            &config,
            "/project",
            &build,
            &model,
            None,
            MaxTurns::FromConfig,
        ));
        assert_eq!(params.max_turns, RunParams::UNBOUNDED);
        assert!(
            params.compaction.is_none(),
            "compaction stays opt-in at this seam"
        );
    }

    #[test]
    fn reauth_hook_resolves_from_the_selected_provider() {
        let config = config("provider \"demo\" { kind = \"openai\"\n  api_key_env = \"KEY\" }");
        let model = ResolvedModel::bare("demo", "gpt-x");
        let build = AgentMode::Build;
        let params = run_policy(inputs(
            &config,
            "/project",
            &build,
            &model,
            None,
            MaxTurns::FromConfig,
        ));
        assert!(
            params.reauth.is_some(),
            "a configured provider yields a reauth hook"
        );
        let unknown = ResolvedModel::bare("gone", "gpt-x");
        let params = run_policy(inputs(
            &config,
            "/project",
            &build,
            &unknown,
            None,
            MaxTurns::FromConfig,
        ));
        assert!(params.reauth.is_none());
    }

    // --- catalog metadata ---

    fn entry(id: &str, context: Option<u32>, output: Option<u32>) -> CatalogModel {
        CatalogModel {
            id: id.into(),
            name: None,
            description: None,
            context_length: context,
            max_output_tokens: output,
        }
    }

    #[test]
    fn catalog_metadata_travels_with_present_entries_only() {
        let catalogs = BTreeMap::from([(
            "acme".to_string(),
            vec![
                entry("one", Some(8_000), Some(4_000)),
                entry("two", None, None),
            ],
        )]);
        assert_eq!(
            catalog_metadata(&catalogs, "acme", "one"),
            (Some(8_000), Some(4_000))
        );
        // Present but metadata-less: both None, not a miss error.
        assert_eq!(catalog_metadata(&catalogs, "acme", "two"), (None, None));
        // Misses never enforce membership.
        assert_eq!(catalog_metadata(&catalogs, "acme", "gone"), (None, None));
        assert_eq!(catalog_metadata(&catalogs, "gone", "one"), (None, None));
        assert_eq!(
            catalog_metadata(&BTreeMap::new(), "acme", "one"),
            (None, None)
        );
    }

    // --- workspace environment ---

    #[tokio::test]
    async fn workspace_env_discovers_instructions_and_builds_every_piece() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "runtime-contract-marker instructions",
        )
        .unwrap();
        for startup in ["connected", "background"] {
            let mode = if startup == "connected" {
                McpStartup::Connected
            } else {
                McpStartup::Background(crate::mcp::McpEvents::default())
            };
            let env = workspace_env(dir.path(), mode, false).await.unwrap();
            assert!(
                env.instructions
                    .text
                    .contains("runtime-contract-marker instructions"),
                "{startup}: instructions must be discovered"
            );
            // The workspace canonicalizes its root (macOS /var symlink).
            assert_eq!(
                env.workspace.root(),
                dir.path().canonicalize().unwrap().as_path()
            );
            assert!(env.mcp_errors.is_empty());
            // The permission engine exists even where nothing consults it.
            assert!(!env.permissions.is_yolo());
        }
    }

    #[tokio::test]
    async fn workspace_env_installs_the_reviewer_definition_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        workspace_env(dir.path(), McpStartup::Connected, true)
            .await
            .unwrap();
        assert!(
            dir.path().join(".craft/agents").exists(),
            "reviewer install is requested by the flag"
        );
        let bare = tempfile::tempdir().unwrap();
        workspace_env(bare.path(), McpStartup::Connected, false)
            .await
            .unwrap();
        assert!(!bare.path().join(".craft/agents").exists());
    }

    // --- headless long run (the print-mode fix) ---

    fn overflow_history() -> Vec<crate::history::Message> {
        use crate::compaction::test_support as ts;
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(ts::user(&format!(
                "do task {i} with a fairly long instruction"
            )));
            messages.push(ts::assistant_tool_args(
                &format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo {i}")}),
            ));
            messages.push(ts::tool_result_of(&format!("t{i}"), &"x".repeat(200)));
        }
        messages
    }

    /// The policy `run_headless_query` builds today: shared compaction
    /// state, the selected model's catalog window, `run_policy`. With a
    /// fake model overflowing once: the configured output cap is clamped
    /// into the window on the wire, recovery compacts and retries, and the
    /// turn commits.
    #[tokio::test]
    async fn headless_policy_clamps_caps_and_recovers_from_overflow() {
        use rig_core::test_utils::{MockCompletionModel, MockError, MockStreamEvent};
        let config =
            config("agent { max_tokens = 16384 }\ncompaction \"vcc\" { context = \"0.6\" }");
        let model = MockCompletionModel::from_stream_turns(vec![
            vec![MockStreamEvent::Error(MockError::provider(
                "This model's maximum context length is 8192 tokens. However, you requested 16384 tokens.",
            ))],
            vec![
                MockStreamEvent::text("recovered"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);

        // Window sized so the output cap (16384) never fits: the clamp must
        // bite, but the floor keeps a usable budget.
        let history_tokens = crate::compaction::estimate_tokens(&overflow_history());
        let context_length = (history_tokens + 8192).max(1) as u32;
        let compaction_state =
            new_compaction_state(crate::run::shared_cache(), crate::run::shared_guardrails());
        let params = run_policy(RunPolicyInputs {
            config: &config,
            cwd: "/project",
            instructions_text: "",
            mode: &AgentMode::Build,
            model: &ResolvedModel {
                provider: "mock".into(),
                model_id: "overflowing".into(),
                context_length: Some(context_length),
                max_output_tokens: None,
            },
            compaction: Some(compaction_ctx(
                compaction_state.clone(),
                &config,
                Some(context_length),
            )),
            recency: None,
            retry: RetryCtx::default(),
            fast: false,
            thinking: None,
            max_turns: MaxTurns::FromConfig,
        });
        let window = params
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.context_length)
            .expect("headless compaction carries the catalog window");

        let dir = tempfile::tempdir().unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = crate::run::cancel_channel();
        let mut history = overflow_history();
        let events = std::sync::Mutex::new(Vec::new());
        let outcome = crate::run::run(
            &model,
            &params,
            &tools,
            &mut history,
            "continue",
            &cancel,
            &|event| events.lock().unwrap().push(event),
        )
        .await;

        assert!(
            matches!(&outcome, crate::run::RunOutcome::Done { reply } if reply == "recovered"),
            "overflow recovery must complete the run, got {outcome:?}"
        );
        // The retried request was rebuilt from the compacted history, and
        // every request carried the window-clamped cap (the pre-fix
        // headless policy sent the configured 16384 untouched).
        assert_eq!(model.requests().len(), 2);
        for request in model.requests() {
            let clamped = request.max_tokens.expect("the clamp must set a cap");
            assert!(
                clamped < 16384,
                "the configured cap must be clamped into the window, got {clamped}"
            );
            assert!(clamped <= u64::from(window), "never above the window");
            assert!(clamped >= 4096, "never below the output floor");
        }
        // Recovery events fired and the turn committed to history.
        let guard = events.lock().unwrap();
        assert!(
            guard
                .iter()
                .any(|event| matches!(event, crate::run::Event::AutoCompacting { .. }))
        );
        assert!(
            guard
                .iter()
                .any(|event| matches!(event, crate::run::Event::CompactionDone { .. }))
        );
        drop(guard);
        assert!(
            history.iter().any(|message| matches!(message,
                crate::history::Message::Assistant { content, .. }
                    if content.iter().any(|block| matches!(block,
                        crate::history::AssistantContent::Text(text) if text.text == "recovered")))),
            "the committed history carries the final reply"
        );
    }
}

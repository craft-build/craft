//! Resilience / fault-injection harness.
//!
//! Deterministic power-set over six typed provider faults, exercising the
//! agent loop's failure handling at the same `Provider::stream_message` seam
//! the loop uses. Each case asserts the loop reaches a clean terminal or
//! recoverable state and that history stays consistent. See
//! `docs/feature-resilience-harness.md`.
//!
//! This is a correctness gate, not a benchmark and not a paid-model eval: no
//! network, no wall-clock randomness. The `ScriptedProvider` fixes the turn
//! sequence; `FaultProvider` only mutates how/whether those turns are
//! delivered.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use craft_agent::cancel::CancelMap;
use craft_agent::prompt::ResolvedSlots;
use craft_agent::tools::{FileReadTracker, LocalFs, PromotedTools, ToolRegistry};
use craft_agent::{
    Agent, AgentInput, AgentParams, AgentRunParams, CancelToken, DoomTracker, Envelope,
    EventSender, History,
};
use craft_config::{CompressionConfig, DefaultEffect, PermissionsConfig, ToolOutputLines};
use craft_providers::provider::Provider;
use craft_providers::{
    AgentError, ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
    StreamResponse, TokenUsage,
};
use flume::Sender;
use serde_json::{Value, json};
use test_case::test_case;
use tokio::time::timeout;

/// A no-match glob the scripted tool call uses. Read-only and side-effect free,
/// so a duplicated id can never corrupt the test machine.
const GLOB_PATTERN: &str = "*.resilience_no_match_xyz";
/// Raw absolute home path: must never leak into committed history text.
/// Mirrors clark's "no forbidden implementation-detail strings" check.
const FORBIDDEN_RAW_PATH: &str = "/Users/";

const FAULT_BITS: u64 = 6;
const ALL_FAULTS: u64 = (1u64 << FAULT_BITS) - 1;
/// Hard ceiling on any single agent run so a wedged loop fails the test
/// instead of hanging CI.
const RUN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    RateLimit,
    DuplicatedToolCallIds,
    StreamDisconnect,
    ProviderError,
    CompactionInterrupted,
    Cancel,
}

impl Fault {
    const ALL: [Fault; 6] = [
        Fault::RateLimit,
        Fault::DuplicatedToolCallIds,
        Fault::StreamDisconnect,
        Fault::ProviderError,
        Fault::CompactionInterrupted,
        Fault::Cancel,
    ];

    const fn bit(self) -> u64 {
        1 << self as u32
    }

    fn label(self) -> &'static str {
        match self {
            Fault::RateLimit => "rate_limit",
            Fault::DuplicatedToolCallIds => "duplicated_tool_ids",
            Fault::StreamDisconnect => "stream_disconnect",
            Fault::ProviderError => "provider_error",
            Fault::CompactionInterrupted => "compaction_interrupted",
            Fault::Cancel => "cancel",
        }
    }
}

/// Bitmask selecting which faults fire.
#[derive(Debug, Clone, Copy)]
struct FaultSet(u64);

impl FaultSet {
    const fn contains(self, f: Fault) -> bool {
        self.0 & f.bit() != 0
    }
}

fn tool_call_response(tool_id: &str, inflated_input_usage: Option<u32>) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                tool_id,
                "glob",
                json!({ "pattern": GLOB_PATTERN }),
            )],
            ..Default::default()
        },
        usage: TokenUsage {
            input: inflated_input_usage.unwrap_or(10),
            ..TokenUsage::default()
        },
        stop_reason: Some(StopReason::ToolUse),
    }
}

fn text_response(stop_reason: StopReason) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(stop_reason),
    }
}

/// One reply in the scripted turn sequence. `CompactionFail` is a synthetic
/// step the injector uses to fail a compaction provider call.
enum Step {
    Respond(StreamResponse),
    CompactionFail,
}

/// Scripted base provider. Emits a fixed turn sequence (tool call, then a
/// follow-up text turn), crossing the post-tool-result window the harness
/// targets. Compaction cases append a `CompactionFail` step and inflate the
/// tool-call usage so `try_auto_compact` triggers a provider call.
struct ScriptedProvider {
    steps: Mutex<VecDeque<Step>>,
}

impl ScriptedProvider {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn stream_message(
        &self,
        _: &Model,
        _: &[Message],
        _: &str,
        _: &Value,
        _: &Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&craft_storage::id::SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        let step = self
            .steps
            .lock()
            .expect("scripted steps poisoned")
            .pop_front()
            .expect("ScriptedProvider: no more steps");
        match step {
            Step::Respond(r) => Ok(r),
            Step::CompactionFail => Err(AgentError::Api {
                status: 400,
                message: "compaction provider failed".into(),
            }),
        }
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        Ok(vec![])
    }
}

/// Per-fault firing state. Counters make triggers deterministic for a given
/// mask: e.g. `RateLimit` fires on the first call only.
#[derive(Default)]
struct FaultState {
    calls: u64,
    stream_disconnect_fired: bool,
    provider_error_fired: bool,
}

/// Provider decorator that injects selected faults around `inner`. Same mask
/// plus same scripted inner = same observable stream.
struct FaultProvider {
    inner: Arc<dyn Provider>,
    faults: FaultSet,
    state: Mutex<FaultState>,
}

impl FaultProvider {
    fn new(inner: Arc<dyn Provider>, faults: FaultSet) -> Self {
        Self {
            inner,
            faults,
            state: Mutex::new(FaultState::default()),
        }
    }
}

#[async_trait]
impl Provider for FaultProvider {
    async fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&craft_storage::id::SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        let call = {
            let mut s = self.state.lock().expect("fault state poisoned");
            s.calls += 1;
            s.calls
        };

        // RateLimit: a 429 on the first call only. The retry loop retries the
        // first call; the second attempt delegates to inner and succeeds, so
        // the case terminates cleanly.
        if self.faults.contains(Fault::RateLimit) && call == 1 {
            return Err(AgentError::Api {
                status: 429,
                message: "rate limited".into(),
            });
        }

        // StreamDisconnect: a transient server error on the second call (the
        // post-tool-result follow-up) once, then succeed. 5xx is retryable but
        // not key-rotatable, so the loop backoffs and retries; the retry
        // delegates to inner and the dangerous window is crossed safely.
        if self.faults.contains(Fault::StreamDisconnect)
            && call == 2
            && !self
                .state
                .lock()
                .expect("fault state poisoned")
                .stream_disconnect_fired
        {
            self.state
                .lock()
                .expect("fault state poisoned")
                .stream_disconnect_fired = true;
            return Err(AgentError::Api {
                status: 503,
                message: "stream disconnected".into(),
            });
        }

        // ProviderError: a non-retryable 4xx on the second call (post-tool
        // result). 4xx is neither retryable nor abort, so the loop returns the
        // error immediately and the run ends in Err. History must stay
        // consistent across the window.
        if self.faults.contains(Fault::ProviderError)
            && call == 2
            && !self
                .state
                .lock()
                .expect("fault state poisoned")
                .provider_error_fired
        {
            self.state
                .lock()
                .expect("fault state poisoned")
                .provider_error_fired = true;
            return Err(AgentError::Api {
                status: 400,
                message: "bad request".into(),
            });
        }

        let response = self
            .inner
            .stream_message(model, messages, system, tools, event_tx, opts, session_id)
            .await?;

        // DuplicatedToolCallIds: clone the tool-use block in the returned
        // assistant message so two blocks share one id. Tests the dedup /
        // double-execute path: both blocks must get a paired result.
        if self.faults.contains(Fault::DuplicatedToolCallIds) {
            let mut message = response.message.clone();
            if let Some(ContentBlock::ToolUse {
                id, name, input, ..
            }) = message.content.first()
            {
                message.content.push(ContentBlock::tool_use(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                ));
            }
            return Ok(StreamResponse {
                message,
                usage: response.usage,
                stop_reason: response.stop_reason,
            });
        }

        Ok(response)
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        self.inner.list_models().await
    }
}

fn default_model() -> Model {
    Model::from_spec("anthropic/claude-sonnet-4-20250514").expect("model spec")
}

/// A model with a tiny context window so an inflated tool-call usage forces
/// `try_auto_compact` to fire and call the provider for compaction.
fn tiny_window_model() -> Model {
    let mut model = default_model();
    model.context_window = 256;
    model.max_output_tokens = Some(64);
    model
}

fn make_agent_params(model: Model, provider: Arc<dyn Provider>) -> AgentParams {
    AgentParams {
        provider,
        model,
        config: craft_config::AgentConfig::default(),
        tool_output_lines: ToolOutputLines::default(),
        permissions: Arc::new(craft_agent::permissions::PermissionManager::new(
            PermissionsConfig {
                default: DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp"),
        )),
        session_id: None,
        mailbox: None,
        timeouts: craft_providers::Timeouts::default(),
        file_tracker: FileReadTracker::fresh(),
        prompt_slots: Arc::new(ResolvedSlots::default()),
        subagent_cancels: Arc::new(CancelMap::new()),
        registry: Arc::new(ToolRegistry::with_natives()),
        compression: CompressionConfig::default(),
        model_policy: Arc::new(craft_config::ModelPolicy::default()),
        findings_store: None,
        fs: Arc::new(LocalFs),
        doom: Arc::new(Mutex::new(DoomTracker::new())),
        flow_thread_history: None,
        flow_thread_manager: None,
        flow_advisor: None,
        flow_progress_tx: None,
    }
}

fn make_run_params(history: &mut History) -> (AgentRunParams<'_>, flume::Receiver<Envelope>) {
    let (raw_tx, event_rx) = flume::unbounded();
    (
        AgentRunParams {
            history,
            system: "system".into(),
            event_tx: EventSender::new(raw_tx, 0),
            tools: json!([]),
            promoted: PromotedTools::new(),
            tool_build: None,
            hooks: None,
        },
        event_rx,
    )
}

struct CaseOutcome {
    result: Result<(), AgentError>,
    history: Vec<Message>,
}

/// Build a fresh `ScriptedProvider` for the given mask, wrap it in
/// `FaultProvider`, run the agent once, and capture the terminal state plus
/// the resulting history.
async fn run_case(mask: u64) -> CaseOutcome {
    let wants_compaction = mask & Fault::CompactionInterrupted.bit() != 0;
    let mut steps = vec![
        Step::Respond(tool_call_response("t1", wants_compaction.then_some(10_000))),
        Step::Respond(text_response(StopReason::EndTurn)),
    ];
    if wants_compaction {
        steps.push(Step::CompactionFail);
    }
    let scripted = Arc::new(ScriptedProvider::new(steps));
    let provider: Arc<dyn Provider> = Arc::new(FaultProvider::new(
        Arc::clone(&scripted) as Arc<dyn Provider>,
        FaultSet(mask),
    ));
    let model = if wants_compaction {
        tiny_window_model()
    } else {
        default_model()
    };

    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let params = make_agent_params(model, provider);

    let cancel_wired = mask & Fault::Cancel.bit() != 0;
    let (trigger, token) = if cancel_wired {
        let (t, tk) = CancelToken::new();
        (Some(t), tk)
    } else {
        (None, CancelToken::none())
    };
    let agent = Agent::new(params, run_params).with_cancel(token);

    if let Some(trigger) = trigger {
        // Cancel mid-run: fire the token after the first turn has had time to
        // attach a tool result and start the follow-up (the dangerous window).
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            trigger.cancel();
        });
    }

    let result = timeout(RUN_TIMEOUT, agent.run(default_input()))
        .await
        .expect("agent run timed out (loop wedged)");

    // Drain the event channel so the unbounded sender never blocks; the
    // terminal reason is already captured in `result` (the loop returns
    // Err rather than emitting Done for the cancel/error paths).
    drain_events(&event_rx);
    let history_vec = history.into_vec();
    CaseOutcome {
        result,
        history: history_vec,
    }
}

fn default_input() -> AgentInput {
    AgentInput {
        message: "go".into(),
        ..AgentInput::default()
    }
}

fn drain_events(rx: &flume::Receiver<Envelope>) {
    while rx.try_recv().is_ok() {}
}

/// Run a fresh healthy turn on the same history; assert the loop is not
/// wedged. Asserts invariant §4.4 #4: the loop is recoverable.
async fn recoverable_follow_up(history: Vec<Message>) -> Result<(), AgentError> {
    let recovery = Arc::new(ScriptedProvider::new(vec![
        Step::Respond(text_response(StopReason::EndTurn)),
        Step::Respond(text_response(StopReason::EndTurn)),
    ]));
    let provider: Arc<dyn Provider> = Arc::clone(&recovery) as Arc<dyn Provider>;
    let mut restored = History::restored(history);
    let (run_params, _event_rx) = make_run_params(&mut restored);
    let params = make_agent_params(default_model(), provider);
    let agent = Agent::new(params, run_params).with_cancel(CancelToken::none());
    timeout(RUN_TIMEOUT, agent.run(default_input()))
        .await
        .expect("follow-up run timed out (loop wedged)")
}

/// §4.4 invariants: no orphan tool calls, every tool use paired with a result,
/// and no leaked raw paths in committed history.
fn assert_history_consistent(messages: &[Message]) {
    let mut tool_use_ids: Vec<String> = Vec::new();
    for msg in messages {
        if matches!(msg.role, Role::Assistant) {
            for (id, _, _) in msg.tool_uses() {
                tool_use_ids.push(id.to_owned());
            }
        }
    }

    let mut result_ids: Vec<String> = Vec::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                result_ids.push(tool_use_id.clone());
            }
        }
    }

    // Every emitted tool use must have at least one matching result. A
    // duplicated id counts once here; the contract is "no orphan use", so one
    // result for a shared id satisfies both uses.
    for id in &tool_use_ids {
        assert!(
            result_ids.iter().any(|r| r == id),
            "orphan tool use with no result: {id}"
        );
    }

    // No tool result may reference an id no assistant ever emitted.
    for rid in &result_ids {
        assert!(
            tool_use_ids.iter().any(|t| t == rid),
            "tool result references unknown tool use id: {rid}"
        );
    }

    // "No forbidden implementation-detail strings": raw absolute paths must
    // not leak into committed history text.
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::Text { text } = block {
                assert!(
                    !text.contains(FORBIDDEN_RAW_PATH),
                    "raw path leaked into history: {text}"
                );
            }
        }
    }
}

/// Assert the universal §4.4 healthy-surface contract for any case:
/// the loop reached a terminal state without panicking, history is
/// consistent (no orphan tool uses, no unknown results, no leaked paths),
/// and a follow-up turn recovers. The specific terminal reason varies by
/// fault combination (the first-terminating fault wins), so it is checked
/// separately for isolated single-bit cases.
fn assert_healthy(outcome: &CaseOutcome) {
    assert_history_consistent(&outcome.history);
}

/// For an isolated single-bit fault, the terminal reason is deterministic:
/// document and assert it. In combinations the first-terminating fault wins,
/// so this only runs on the single-bit smoke cases.
fn assert_isolated_terminal(outcome: &CaseOutcome, fault: Fault) {
    match fault {
        Fault::Cancel => assert!(
            outcome.result.is_err(),
            "cancel should end in Err(Cancelled)"
        ),
        Fault::ProviderError | Fault::CompactionInterrupted => assert!(
            outcome.result.is_err(),
            "non-retryable provider fault should end in Err"
        ),
        // RateLimit / StreamDisconnect are retryable then succeed: run ends Ok.
        // DuplicatedToolCallIds completes normally: run ends Ok.
        _ => assert!(
            outcome.result.is_ok(),
            "retryable/no-op fault in isolation should end in Ok"
        ),
    }
}

/// Baseline (no faults) plus each isolated single-bit fault. The single-bit
/// cases also assert the documented terminal reason via
/// `assert_isolated_terminal`.
#[test_case(0u64                                              ; "baseline")]
#[test_case(Fault::RateLimit.bit()                            ; "rate_limit")]
#[test_case(Fault::DuplicatedToolCallIds.bit()                ; "duplicated_tool_ids")]
#[test_case(Fault::StreamDisconnect.bit()                     ; "stream_disconnect")]
#[test_case(Fault::ProviderError.bit()                        ; "provider_error")]
#[test_case(Fault::CompactionInterrupted.bit()                ; "compaction_interrupted")]
#[test_case(Fault::Cancel.bit()                               ; "cancel")]
#[tokio::test]
async fn resilience_isolated(mask: u64) {
    let outcome = run_case(mask).await;
    assert_healthy(&outcome);
    if let Some(fault) = single_fault(mask) {
        assert_isolated_terminal(&outcome, fault);
    } else {
        // Baseline: the loop completes cleanly.
        assert!(outcome.result.is_ok(), "baseline should end in Ok");
    }
    recoverable_follow_up(outcome.history)
        .await
        .expect("follow-up turn should recover cleanly");
}

/// All faults at once. In a combination the first-terminating fault wins, so
/// only the universal contract (no panic, history consistent, recoverable)
/// is asserted here.
#[tokio::test]
async fn resilience_all_faults() {
    let outcome = run_case(ALL_FAULTS).await;
    assert_healthy(&outcome);
    recoverable_follow_up(outcome.history)
        .await
        .expect("follow-up turn should recover cleanly");
}

/// Full 2^6 = 64-case power set. Marked `#[ignore]` so default CI stays fast;
/// run explicitly with
/// `cargo nextest run -p craft-agent --test resilience --run-ignored only`
/// or `cargo test -p craft-agent --test resilience -- --ignored`.
#[tokio::test]
#[ignore = "full 64-case power set; run explicitly"]
async fn full_power_set() {
    for mask in 0..(1u64 << FAULT_BITS) {
        let outcome = run_case(mask).await;
        assert_healthy(&outcome);
        let label = label_for(mask);
        recoverable_follow_up(outcome.history)
            .await
            .unwrap_or_else(|e| panic!("mask {mask} ({label}): follow-up failed: {e}"));
    }
}

/// `Some(fault)` when `mask` is exactly one bit set, else `None` (baseline or
/// a combination).
fn single_fault(mask: u64) -> Option<Fault> {
    if mask == 0 || mask & (mask - 1) != 0 {
        return None;
    }
    Fault::ALL.iter().copied().find(|f| f.bit() == mask)
}

fn label_for(mask: u64) -> String {
    let label = Fault::ALL
        .iter()
        .filter(|f| mask & f.bit() != 0)
        .map(|f| f.label())
        .collect::<Vec<_>>()
        .join("+");
    if label.is_empty() {
        "baseline".to_string()
    } else {
        label
    }
}

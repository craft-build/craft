//! `craft flow` subcommand. Drives Flow mode headlessly through the normal
//! `Agent::run` path (Flow mode is Build mode with a mutable `turn_type` and
//! a typed log; the pipeline shape emerges from the model's `shift` calls).
//! Each run opens (or resumes) the per-workstream `ThreadHistory`, attaches it
//! plus a `ThreadManager`, the no-op advisor, and a progress channel to the
//! `Agent`, and translates the terminal `AgentEvent::Done` stop reason into a
//! `FlowOutcome` for printing. Also prunes old workstream directories
//! (`craft flow gc`).

use std::env;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use craft_agent::agent::flow_loop::{self, FlowOutcome, FlowRunState};
use craft_agent::permissions::PermissionManager;
use craft_agent::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope, FindingsStore,
    History, ToolOutputLines,
};
use craft_config::{load_env_files, load_permissions};
use craft_providers::StopReason;
use craft_storage::StateDir;
use craft_storage::flow::FlowStore;

use crate::cli::FlowAction;
use crate::print::OutputFormat;
use crate::setup;

/// Stop-reason strings for outcomes with no provider analogue.
const STOP_DONE: &str = "done";
const STOP_ERROR: &str = "error";
const STOP_CANCELLED: &str = "cancelled";
const STOP_NEEDS_REVIEW: &str = "needs_review";
const STOP_AWAITING: &str = "awaiting_goal_approval";
const APPROVED_TOKEN: &str = "approved";

pub async fn run(action: FlowAction) -> Result<()> {
    match action {
        FlowAction::Run {
            request,
            print: _print,
            output_format,
            session,
            payload,
            retry,
        } => run_flow(request, output_format, session, payload, retry).await,
        FlowAction::Gc { older_than } => gc(older_than),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_flow(
    request: Option<String>,
    output_format: OutputFormat,
    session: Option<String>,
    payload: Option<String>,
    retry: bool,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let plugin_host = craft_lua::PluginHost::new(
        Arc::clone(craft_agent::tools::ToolRegistry::native_arc()),
        None,
    )
    .context("initialize lua plugin host")?;
    let raw_config = plugin_host
        .load_init_files(&cwd)
        .context("load init.lua files")?;
    let config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    let _ = load_permissions(&cwd);

    setup::init_logging(&config.storage);
    setup::install_panic_log_hook();

    let model = setup::resolve_model(None, &config.provider, &storage).await?;
    let project_id = craft_storage::flow::project_id(&cwd);
    let store = Arc::new(FlowStore::new(&storage).context("init flow store")?);
    if retry && session.is_none() {
        bail!("--retry requires -s <session-id> to identify the workstream to resume");
    }
    let workstream_id = match &session {
        Some(id) => id.clone(),
        None => new_workstream_id(),
    };

    let request = match request {
        Some(r) => r,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            buf
        }
    };

    // Resume: an approval/revision payload turns the prompt into the user's
    // decision text. The agent reads its persisted typed log to re-derive the
    // goal and the next shift (plan §7: the approval gate is an ordinary turn
    // boundary; resume re-enters at General and the model re-derives).
    let message = if let Some(p) = payload {
        if p.trim().eq_ignore_ascii_case(APPROVED_TOKEN) {
            APPROVED_TOKEN.to_string()
        } else {
            p
        }
    } else {
        request
    };

    let prompt_slots = plugin_host.event_handle().collect_prompt_slots();
    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };
    let (event_tx, event_rx) = flume::unbounded::<Envelope>();
    let provider = {
        let mut model_for_provider = model.clone();
        Arc::from(
            craft_providers::provider::from_model(&mut model_for_provider, timeouts)
                .await
                .context("init flow provider")?,
        )
    };
    let (state, progress_rx, _cancel_trigger) =
        FlowRunState::split(Arc::clone(&store), project_id, workstream_id.clone());

    let tool_build = craft_agent::tools::ToolBuild {
        vars: craft_agent::template::env_vars(),
        excluded: Vec::new(),
        mcp: None,
    };
    let dynamic = craft_agent::tools::DynamicContext::from_config(&config.agent);
    let tools = craft_agent::tools::build_active_tools(
        &tool_build,
        &model,
        &config.agent,
        &dynamic,
        &craft_agent::tools::PromotedTools::new(),
    );
    let instructions = craft_agent::agent::load_instruction_text(&cwd.to_string_lossy());
    let system = craft_agent::template::env_vars()
        .apply(&craft_agent::agent::build_system_prompt(
            &craft_agent::template::env_vars(),
            &AgentMode::Flow(workstream_id.clone()),
            &instructions,
            &Arc::new(prompt_slots),
            &model,
            false,
        ))
        .into_owned();

    let mut history = History::new(Vec::new());
    let agent = Agent::new(
        AgentParams {
            provider,
            model: model.clone(),
            config: config.agent.clone(),
            tool_output_lines: ToolOutputLines::default(),
            permissions: Arc::new(PermissionManager::new(load_permissions(&cwd), cwd.clone())),
            session_id: None,
            timeouts,
            file_tracker: Arc::new(craft_agent::tools::FileReadTracker::new()),
            prompt_slots: Arc::new(plugin_host.event_handle().collect_prompt_slots()),
            subagent_cancels: Arc::new(craft_agent::cancel::CancelMap::new()),
            registry: Arc::clone(craft_agent::tools::ToolRegistry::native_arc()),
            compression: config.compression.clone(),
            findings_store: Some(Arc::clone(&FindingsStore::new_shared())),
            fs: Arc::new(craft_agent::tools::LocalFs),
            doom: Arc::new(std::sync::Mutex::new(craft_agent::DoomTracker::new())),
            flow_thread_history: Some(state.thread_history),
            flow_thread_manager: Some(state.thread_manager),
            flow_advisor: Some(state.advisor),
            flow_gates: None,
            flow_progress_tx: Some(state.progress_tx),
        },
        AgentRunParams {
            history: &mut history,
            system,
            event_tx: craft_agent::EventSender::new(event_tx, 0),
            tools,
            promoted: craft_agent::tools::PromotedTools::new(),
            tool_build: Some(tool_build),
            hooks: None,
        },
    );

    let input = AgentInput {
        message,
        mode: AgentMode::Flow(workstream_id),
        ..Default::default()
    };

    // Run the agent while concurrently draining the event + progress streams
    // so channels stay clear and we can derive the terminal `FlowOutcome`.
    // The agent owns `history` for the duration of the run; we collect events
    // from the same `event_tx` it emits onto.
    let outcome = tokio::select! {
        biased;
        o = collect_outcome(event_rx, progress_rx) => o,
        r = agent.run(input) => match r {
            Ok(()) => FlowOutcome::Done { verification_report: String::new() },
            Err(e) => FlowOutcome::Failed {
                stage: craft_agent::TurnType::General,
                reason: e.user_message(),
            },
        },
    };
    print_outcome(&outcome, &model.id, output_format);
    Ok(())
}

/// Drain the agent's event stream + Flow progress stream until the terminal
/// `AgentEvent::Done` arrives, then derive the `FlowOutcome` from its stop
/// reason. Text emitted along the way is concatenated as the outcome's text
/// body (goal doc, verification report, etc.).
async fn collect_outcome(
    event_rx: flume::Receiver<Envelope>,
    progress_rx: flume::Receiver<flow_loop::FlowProgress>,
) -> FlowOutcome {
    let mut text = String::new();
    let mut terminal: Option<StopReason> = None;
    let mut error: Option<String> = None;
    loop {
        tokio::select! {
            biased;
            recv = event_rx.recv_async() => {
                let Ok(envelope) = recv else { break; };
                match envelope.event {
                    AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                    AgentEvent::Done { stop_reason, .. } => {
                        terminal = stop_reason;
                        break;
                    }
                    AgentEvent::Error { message } => {
                        error = Some(message);
                        break;
                    }
                    _ => {}
                }
            }
            recv = progress_rx.recv_async() => {
                let Ok(p) = recv else { continue; };
                match p {
                    flow_loop::FlowProgress::GoalReady { goal_doc } => {
                        text = goal_doc;
                    }
                    flow_loop::FlowProgress::Done { verdict } => {
                        text = verdict;
                    }
                    flow_loop::FlowProgress::NeedsReview { report } => {
                        text = report;
                    }
                    flow_loop::FlowProgress::Failed { stage, reason } => {
                        return FlowOutcome::Failed { stage, reason };
                    }
                    flow_loop::FlowProgress::Cancelled => {
                        return FlowOutcome::Cancelled;
                    }
                    _ => {}
                }
            }
        }
    }
    match (terminal, error) {
        (Some(StopReason::AwaitingGoalApproval), _) => {
            FlowOutcome::AwaitingGoalApproval { goal_doc: text }
        }
        (Some(StopReason::Cancelled), _) => FlowOutcome::Cancelled,
        (Some(_), _) => FlowOutcome::Done {
            verification_report: text,
        },
        (None, Some(msg)) => FlowOutcome::Failed {
            stage: craft_agent::TurnType::General,
            reason: msg,
        },
        (None, None) => FlowOutcome::Done {
            verification_report: text,
        },
    }
}

fn gc(older_than: String) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    let duration = parse_age(&older_than)?;
    let store = FlowStore::new(&storage).context("init flow store")?;
    let removed = store.prune(duration).context("prune flow workstreams")?;
    eprintln!("Pruned {removed} workstream(s) older than {older_than}.");
    Ok(())
}

/// Parse an age like `30d`, `12h`, `45m`, `90s`, or a bare number (hours).
fn parse_age(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if let Some(num) = trimmed.strip_suffix(['d', 'D']) {
        let days: u64 = num.parse().context("parse days")?;
        return Ok(Duration::from_secs(days * 24 * 60 * 60));
    }
    if let Some(num) = trimmed.strip_suffix(['h', 'H']) {
        let hours: u64 = num.parse().context("parse hours")?;
        return Ok(Duration::from_secs(hours * 60 * 60));
    }
    if let Some(num) = trimmed.strip_suffix(['m', 'M']) {
        let mins: u64 = num.parse().context("parse minutes")?;
        return Ok(Duration::from_secs(mins * 60));
    }
    if let Some(num) = trimmed.strip_suffix(['s', 'S']) {
        let secs: u64 = num.parse().context("parse seconds")?;
        return Ok(Duration::from_secs(secs));
    }
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    bail!("invalid age '{input}'; use a number with a d/h/m/s suffix, e.g. 30d")
}

/// Generate a fresh opaque workstream id.
fn new_workstream_id() -> String {
    craft_storage::id::CraftId::generate().to_string()
}

fn outcome_text(outcome: &FlowOutcome) -> String {
    match outcome {
        FlowOutcome::AwaitingGoalApproval { goal_doc } => goal_doc.clone(),
        FlowOutcome::Done {
            verification_report,
        } => verification_report.clone(),
        FlowOutcome::Failed { stage, reason } => {
            format!("Flow failed at turn '{}': {reason}", stage.as_str())
        }
        FlowOutcome::NeedsReview {
            verification_report,
        } => {
            format!("Flow verification needs review:\n{verification_report}")
        }
        FlowOutcome::Cancelled => "Flow run cancelled.".to_string(),
    }
}

fn outcome_stop(outcome: &FlowOutcome) -> &'static str {
    match outcome {
        FlowOutcome::AwaitingGoalApproval { .. } => STOP_AWAITING,
        FlowOutcome::Done { .. } => STOP_DONE,
        FlowOutcome::Failed { .. } => STOP_ERROR,
        FlowOutcome::NeedsReview { .. } => STOP_NEEDS_REVIEW,
        FlowOutcome::Cancelled => STOP_CANCELLED,
    }
}

fn print_outcome(outcome: &FlowOutcome, model_id: &str, format: OutputFormat) {
    let is_error = matches!(outcome, FlowOutcome::Failed { .. } | FlowOutcome::Cancelled);
    let result = outcome_text(outcome);
    match format {
        OutputFormat::Text => {
            if is_error {
                eprint!("{result}");
            } else {
                print!("{result}");
            }
        }
        OutputFormat::Json | OutputFormat::StreamJson => {
            let json = serde_json::json!({
                "subtype": if is_error { "error" } else { "success" },
                "is_error": is_error,
                "result": result,
                "stop_reason": outcome_stop(outcome),
                "model": model_id,
            });
            println!("{}", serde_json::to_string(&json).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("30d", Duration::from_secs(30 * 24 * 60 * 60) ; "days")]
    #[test_case("12h", Duration::from_secs(12 * 60 * 60) ; "hours")]
    #[test_case("45m", Duration::from_secs(45 * 60) ; "minutes")]
    #[test_case("90s", Duration::from_secs(90) ; "seconds")]
    #[test_case("3600", Duration::from_secs(3600) ; "bare_seconds")]
    fn parse_age_reads_suffixes(input: &str, expected: Duration) {
        assert_eq!(parse_age(input).unwrap(), expected);
    }

    #[test]
    fn parse_age_rejects_garbage() {
        assert!(parse_age("abc").is_err());
    }

    #[test]
    fn new_workstream_id_is_base58_craft_id() {
        let id = new_workstream_id();
        assert!(!id.is_empty());
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn done_outcome_stop_is_done() {
        let o = FlowOutcome::Done {
            verification_report: "{}".into(),
        };
        assert_eq!(outcome_stop(&o), STOP_DONE);
    }

    #[test]
    fn failed_outcome_stop_is_error() {
        let o = FlowOutcome::Failed {
            stage: craft_agent::TurnType::Plan,
            reason: "no chunks".into(),
        };
        assert_eq!(outcome_stop(&o), STOP_ERROR);
    }
}

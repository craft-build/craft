//! `craft flow` subcommand: runs the Flow multi-stage pipeline headlessly,
//! or prunes old workstream directories (`craft flow gc`).

use std::env;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use craft_agent::permissions::PermissionManager;
use craft_agent::tools::FlowRunnerEnv;
use craft_config::{load_env_files, load_permissions};
use craft_flow::{ApprovalPayload, FlowOutcome, FlowParams, TaskStageRunner};
use craft_lua::PluginHost;
use craft_providers::StopReason;
use craft_providers::provider;
use craft_storage::StateDir;
use craft_storage::flow::FlowStore;

use crate::cli::FlowAction;
use crate::print::OutputFormat;
use crate::setup;

/// Stop-reason strings emitted in `--print --output-format json` output for
/// outcomes that don't map to a real provider [`StopReason`]. The
/// `awaiting_goal_approval` gate pause uses the typed
/// `StopReason::AwaitingGoalApproval` variant; `done` and `error` are
/// Flow-local terminal states with no provider analogue.
const STOP_DONE: &str = "done";
const STOP_ERROR: &str = "error";
const STOP_CANCELLED: &str = "cancelled";
const APPROVED_TOKEN: &str = "approved";

pub async fn run(action: FlowAction) -> Result<()> {
    match action {
        FlowAction::Run {
            request,
            print,
            output_format,
            session,
            payload,
        } => run_pipeline(request, print, output_format, session, payload).await,
        FlowAction::Gc { older_than } => gc(older_than),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    request: Option<String>,
    print: bool,
    output_format: OutputFormat,
    session: Option<String>,
    payload: Option<String>,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let plugin_host = PluginHost::new(
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

    setup::init_logging(&storage, &config.storage);
    setup::install_panic_log_hook();

    let model = setup::resolve_model(None, &config.provider, &storage).await?;
    let project_id = craft_flow::project_id(&cwd);
    let store = Arc::new(FlowStore::new(&storage).context("init flow store")?);
    // Resume re-uses the persisted workstream id; a fresh run mints one.
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

    let approval = payload.map(|p| {
        if p.trim().eq_ignore_ascii_case(APPROVED_TOKEN) {
            ApprovalPayload::Approved
        } else {
            ApprovalPayload::Revised(p)
        }
    });

    let mut params = FlowParams::new(
        project_id.clone(),
        workstream_id.clone(),
        request,
        config.agent.flow.clone(),
        Arc::clone(&store),
    );
    params.approval = approval;
    // Live stage runner: launches each stage as a real subagent via Agent::run
    // (model-role resolution, worktree isolation, output-schema validation),
    // closing the gap where the pipeline previously only ran under the
    // deterministic provider-free DefaultRunner. The event channel surfaces
    // stage subagent events for `--print --output-format stream-json` consumers.
    let prompt_slots = plugin_host
        .event_handle()
        .map(|h| h.collect_prompt_slots())
        .unwrap_or_default();
    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };
    let (event_tx, _event_rx) = flume::unbounded::<craft_agent::Envelope>();
    let provider = {
        let mut model_for_provider = model.clone();
        Arc::from(
            provider::from_model(&mut model_for_provider, timeouts)
                .await
                .context("init flow provider")?,
        )
    };
    let embedder: Arc<dyn craft_flow::search::Embedder> = Arc::new(
        craft_flow::search::OnnxEmbedder::new(craft_agent::EmbeddingService::new()),
    );
    params.embedder = Some(Arc::clone(&embedder));
    let flow_search: craft_agent::tools::flow_search::FlowSearchHandle =
        Some(Arc::new(craft_flow::search::FlowSearchBackendImpl::new(
            Arc::clone(&store),
            embedder,
            &project_id,
            &workstream_id,
        )));
    let env = Arc::new(FlowRunnerEnv {
        provider,
        model: Arc::new(model.clone()),
        config: config.agent.clone(),
        permissions: Arc::new(PermissionManager::new(load_permissions(&cwd), cwd.clone())),
        timeouts,
        compression: config.compression.clone(),
        prompt_slots: Arc::new(prompt_slots),
        event_tx: craft_agent::EventSender::new(event_tx, 0),
        flow_search,
    });
    params.runner = Some(Arc::new(TaskStageRunner::new(env, workstream_id)));
    let outcome = craft_flow::run(params).await;

    if print || matches!(output_format, OutputFormat::Json | OutputFormat::StreamJson) {
        print_outcome(&outcome, &model.id, output_format);
    } else {
        print_outcome_text(&outcome);
    }
    Ok(())
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

/// Generate a fresh opaque workstream id. Uses uuid v4 to match session id
/// formatting.
fn new_workstream_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn outcome_stop_reason(outcome: &FlowOutcome) -> StopReasonOrStr {
    match outcome {
        FlowOutcome::AwaitingGoalApproval { .. } => {
            StopReasonOrStr::Typed(StopReason::AwaitingGoalApproval)
        }
        FlowOutcome::Done { .. } => StopReasonOrStr::Str(STOP_DONE),
        FlowOutcome::Failed { .. } => StopReasonOrStr::Str(STOP_ERROR),
        FlowOutcome::Cancelled => StopReasonOrStr::Str(STOP_CANCELLED),
    }
}

/// The approval gate maps to a typed [`StopReason`]; the terminal `done` and
/// `error` outcomes have no provider analogue and stay as plain strings.
enum StopReasonOrStr {
    Typed(StopReason),
    Str(&'static str),
}

fn outcome_text(outcome: &FlowOutcome) -> String {
    match outcome {
        FlowOutcome::AwaitingGoalApproval { goal_doc } => goal_doc.clone(),
        FlowOutcome::Done {
            verification_report,
        } => verification_report.clone(),
        FlowOutcome::Failed { stage, reason } => {
            format!("Flow failed at stage '{}': {reason}", stage.as_str())
        }
        FlowOutcome::Cancelled => "Flow run cancelled.".to_string(),
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
            let json = outcome_json(outcome, model_id);
            println!("{}", serde_json::to_string(&json).unwrap_or_default());
        }
    }
}

/// Build the JSON object emitted for `--output-format json`. Extracted from
/// [`print_outcome`] so the stop-reason serialization is unit-testable without
/// capturing stdout.
fn outcome_json(outcome: &FlowOutcome, model_id: &str) -> serde_json::Value {
    let is_error = matches!(outcome, FlowOutcome::Failed { .. } | FlowOutcome::Cancelled);
    let result = outcome_text(outcome);
    let stop_reason = match outcome_stop_reason(outcome) {
        StopReasonOrStr::Typed(sr) => serde_json::to_value(sr).unwrap_or_default(),
        StopReasonOrStr::Str(s) => serde_json::Value::from(s),
    };
    serde_json::json!({
        "subtype": if is_error { "error" } else { "success" },
        "is_error": is_error,
        "result": result,
        "stop_reason": stop_reason,
        "model": model_id,
    })
}

fn print_outcome_text(outcome: &FlowOutcome) {
    let text = outcome_text(outcome);
    if matches!(outcome, FlowOutcome::Failed { .. } | FlowOutcome::Cancelled) {
        eprintln!("{text}");
    } else {
        println!("{text}");
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
    fn new_workstream_id_is_hex() {
        let id = new_workstream_id();
        // uuid v4 string with hyphens, e.g. "550e8400-e29b-41d4-a716-446655440000".
        assert_eq!(id.len(), 36);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn approval_gate_json_emits_typed_stop_reason() {
        let outcome = FlowOutcome::AwaitingGoalApproval {
            goal_doc: "goal: ship it".to_string(),
        };
        let json = outcome_json(&outcome, "test-model");
        assert_eq!(
            json["stop_reason"],
            serde_json::json!("awaiting_goal_approval")
        );
        assert_eq!(json["is_error"], serde_json::json!(false));
        assert_eq!(json["result"], serde_json::json!("goal: ship it"));
    }

    #[test]
    fn done_outcome_json_emits_done_stop_reason() {
        let outcome = FlowOutcome::Done {
            verification_report: "{\"goal_met\":true}".to_string(),
        };
        let json = outcome_json(&outcome, "m");
        assert_eq!(json["stop_reason"], serde_json::json!("done"));
    }

    #[test]
    fn failed_outcome_json_emits_error_stop_reason() {
        let outcome = FlowOutcome::Failed {
            stage: craft_flow::Stage::Plan,
            reason: "no chunks".to_string(),
        };
        let json = outcome_json(&outcome, "m");
        assert_eq!(json["stop_reason"], serde_json::json!("error"));
        assert_eq!(json["is_error"], serde_json::json!(true));
    }

    #[test]
    fn cancelled_outcome_json_emits_cancelled_stop_reason() {
        let outcome = FlowOutcome::Cancelled;
        let json = outcome_json(&outcome, "m");
        assert_eq!(json["stop_reason"], serde_json::json!("cancelled"));
        assert_eq!(json["is_error"], serde_json::json!(true));
        assert_eq!(json["result"], serde_json::json!("Flow run cancelled."));
    }
}

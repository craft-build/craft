use std::env;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::Context;

use craft_agent::headless::{self, HeadlessHandle, HeadlessParams};
use craft_agent::tools::{QUESTION_TOOL_NAME, ToolRegistry};
use craft_agent::{AgentEvent, Envelope};
use craft_config::{load_env_files, load_permissions};
use craft_lua::PluginHost;
use craft_providers::Message;
use craft_providers::{StopReason, Timeouts, TokenUsage};
use craft_storage::StateDir;
use craft_storage::sessions::Session;

use crate::print::OutputFormat;
use crate::setup;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub struct HeadlessOptions {
    pub model: Option<String>,
    pub prompt: String,
    pub yolo: bool,
    pub no_plugins: bool,
    pub no_rtk: bool,
    pub extra_excluded_tools: Vec<&'static str>,
    /// Extra environment context (e.g. shell history) injected before the prompt.
    pub context: Vec<String>,
    /// When true, persist a session log reconstructed from turn/tool-result events.
    pub persist_session: bool,
    /// Override the agent max-turns limit.
    pub max_turns: Option<u32>,
    /// Pre-approve only these tools (snake_case or PascalCase); unknown ones are dropped.
    pub allowed_tools: Vec<String>,
    /// Stream assistant text and tool activity to the terminal as it happens.
    pub stream: bool,
}

pub struct HeadlessOutcome {
    pub text: String,
    pub usage: TokenUsage,
    pub num_turns: u32,
    pub stop_reason: Option<StopReason>,
    pub is_error: bool,
    pub session_id: String,
    pub model_id: String,
    /// True once any assistant text has been streamed live to stdout, so the
    /// final print can avoid duplicating it.
    pub streamed_text: bool,
}

/// Run a single headless agent query end-to-end: resolve config, model, provider,
/// and MCP, spawn the agent, drain its events, and return the assembled outcome.
pub async fn run_headless(opts: HeadlessOptions) -> Result<HeadlessOutcome> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let plugin_host = if opts.no_plugins {
        PluginHost::disabled()
    } else {
        PluginHost::new(Arc::clone(ToolRegistry::native_arc()), None)
            .context("initialize lua plugin host")?
    };

    let raw_config = plugin_host
        .load_init_files(&cwd)
        .context("load init.lua files")?;
    let mut config = raw_config
        .unwrap_or_default()
        .into_config(opts.no_rtk)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);

    if opts.yolo || config.always_yolo {
        config.permissions.yolo = true;
        config.sandbox.mode = craft_config::SandboxMode::Off;
        config.sandbox.enabled = false;
    }
    config.validate()?;

    if let Some(handle) = plugin_host.event_handle().as_ref() {
        handle.set_sandbox_config(config.sandbox.clone());
    }

    let timeouts = Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(opts.model.as_deref(), &config.provider, &storage)
        .await
        .context("resolve model")?;

    setup::init_logging(&storage, &config.storage);
    setup::install_panic_log_hook();

    let prompt_slots = plugin_host
        .event_handle()
        .as_ref()
        .map(|h| h.collect_prompt_slots())
        .unwrap_or_default();

    let (mcp_handle, mcp_config_errors) = craft_agent::mcp::start(&cwd).await;
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }

    let fast = config.always_fast && model.supports_fast();

    let mut excluded = vec![QUESTION_TOOL_NAME];
    excluded.extend(opts.extra_excluded_tools);

    let prompt = inject_context(&opts.prompt, &opts.context);
    let model_id = model.id.clone();

    let mut agent_config = config.agent;
    if let Some(max) = opts.max_turns {
        agent_config.max_turns = Some(max);
    }
    if !opts.allowed_tools.is_empty() {
        agent_config.allowed_tools = opts
            .allowed_tools
            .iter()
            .filter_map(|t| crate::cli::normalize_tool_name(t).ok())
            .collect();
    }

    let handle: HeadlessHandle = headless::spawn(HeadlessParams {
        model,
        config: agent_config,
        compression: config.compression,
        permissions_config: config.permissions,
        timeouts,
        prompt: prompt.clone(),
        prompt_slots,
        excluded_tools: excluded,
        mcp_handle,
        initial_wd: cwd.clone(),
        fast,
    });

    let outcome = drain(
        handle,
        opts.persist_session,
        opts.stream,
        &storage,
        &prompt,
        &model_id,
    )
    .await;
    Ok(outcome)
}

fn inject_context(prompt: &str, context: &[String]) -> String {
    if context.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::from("<context>\n");
    for c in context {
        out.push_str(c);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("</context>\n\n");
    out.push_str(prompt);
    out
}

async fn drain(
    handle: HeadlessHandle,
    persist_session: bool,
    stream: bool,
    storage: &StateDir,
    prompt: &str,
    model_id: &str,
) -> HeadlessOutcome {
    let HeadlessHandle {
        event_rx,
        session_id,
        task,
        ..
    } = handle;

    let mut outcome = HeadlessOutcome {
        text: String::new(),
        usage: TokenUsage::default(),
        num_turns: 0,
        stop_reason: None,
        is_error: false,
        session_id: session_id.clone(),
        model_id: model_id.to_string(),
        streamed_text: false,
    };

    let mut session_messages: Vec<Message> = Vec::new();
    if persist_session {
        session_messages.push(Message::user(prompt.to_string()));
    }

    while let Ok(envelope) = event_rx.recv_async().await {
        let Envelope {
            ref event,
            ref subagent,
            ..
        } = envelope;
        let is_top_level = subagent.is_none();
        match event {
            AgentEvent::TextDelta { text } if is_top_level => {
                outcome.text.push_str(text);
                if stream {
                    print!("{text}");
                    let _ = io::stdout().flush();
                    outcome.streamed_text = true;
                }
            }
            AgentEvent::TurnComplete(tc) if is_top_level && persist_session => {
                session_messages.push(tc.message.clone());
            }
            AgentEvent::ToolResultsSubmitted { message } if is_top_level && persist_session => {
                session_messages.push(message.as_ref().clone());
            }
            AgentEvent::Done {
                usage,
                num_turns,
                stop_reason,
            } => {
                outcome.usage = *usage;
                outcome.num_turns = *num_turns;
                outcome.stop_reason = *stop_reason;
                break;
            }
            AgentEvent::Error { message } => {
                outcome.is_error = true;
                outcome.text = message.clone();
                break;
            }
            AgentEvent::ToolStart(ev) if is_top_level && stream => {
                if ev.summary.is_empty() {
                    eprintln!("  ▸ {}", &*ev.tool);
                } else {
                    eprintln!("  ▸ {}: {}", &*ev.tool, ev.summary);
                }
            }
            _ => {}
        }
    }

    tokio::select! {
        _ = task => {}
        _ = tokio::time::sleep(SHUTDOWN_TIMEOUT) => {}
    }

    if persist_session && !session_messages.is_empty() {
        let mut session =
            Session::<Message, TokenUsage, craft_agent::ToolOutput>::new(model_id, "");
        session.id = session_id;
        session.messages = session_messages;
        session.token_usage = outcome.usage;
        session.update_title_if_default();
        let _ = session.save(storage);
    }

    outcome
}

/// Print a headless outcome as text or JSON.
pub fn print_outcome(outcome: &HeadlessOutcome, format: OutputFormat) {
    match format {
        OutputFormat::Text => {
            if outcome.is_error || !outcome.streamed_text {
                print!("{}", outcome.text);
            }
        }
        OutputFormat::Json | OutputFormat::StreamJson => {
            let result = serde_json::json!({
                "subtype": if outcome.is_error { "error" } else { "success" },
                "is_error": outcome.is_error,
                "result": &outcome.text,
                "session_id": &outcome.session_id,
                "model": &outcome.model_id,
                "num_turns": outcome.num_turns,
                "stop_reason": &outcome.stop_reason,
                "usage": &outcome.usage,
            });
            println!("{}", serde_json::to_string(&result).unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_context_wraps_context_block() {
        let out = inject_context("do thing", &["hist1".into(), "hist2".into()]);
        assert!(out.starts_with("<context>\n"));
        assert!(out.contains("hist1"));
        assert!(out.contains("hist2"));
        assert!(out.ends_with("do thing"));
    }

    #[test]
    fn inject_context_passthrough_when_empty() {
        let out = inject_context("do thing", &[]);
        assert_eq!(out, "do thing");
    }
}

//! Craft's base agent loop, using Rig's native runtime.
//!
//! No parallel conversation state, retry engine, or tool dispatcher lives here.
//! Rig's `AgentRunner` drives model calls, hooks, and future tool execution.
//!
//! ```no_run
//! use craft_acp::{agent, config::Config, providers::Provider};
//! use rig::completion::{Chat, Message};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = Config::load().await?;
//! let provider = Provider::from_config(&config.providers["openai"])?;
//! let agent = agent::build(&provider, "gpt-5.2", &config.agent)?;
//!
//! // Caller-owned history; Rig appends committed messages after a successful run.
//! let mut history = Vec::<Message>::new();
//! let reply = agent.chat("Help me plan a refactor.", &mut history).await?;
//!
//! // Use the runner for structured content, token usage, and per-run hooks.
//! let response = agent.runner("Summarize the plan.")
//!     .history(history)
//!     .run()
//!     .await?;
//! println!("{}", response.output());
//! # Ok(())
//! # }
//! ```

use anyhow::Result;
use rig::agent::{Agent, AgentBuilder, ModelHandle};

use crate::{config::AgentConfig, providers::Provider};

/// Build a tool-free agent for an explicit provider and exact model ID.
///
/// Call inside a Tokio runtime: Rig starts its tool-server task on build, even
/// with no tools registered. Construction performs no model discovery or inference.
/// The returned native agent supports `runner`, `Prompt`, `Chat`, and streaming.
pub fn build(provider: &Provider, model: &str, config: &AgentConfig) -> Result<Agent> {
    Ok(builder(provider, model, config)?.build())
}

/// Configure the native builder, leaving Rig's hook/tool extension points open.
///
/// Unknown-to-discovery model IDs are allowed; the provider validates them on
/// the first request. Non-completion providers (e.g. Voyage AI) are rejected.
pub fn builder(provider: &Provider, model: &str, config: &AgentConfig) -> Result<AgentBuilder> {
    config.validate()?;
    Ok(configure(provider.completion_model(model)?, config))
}

fn configure(model: ModelHandle, config: &AgentConfig) -> AgentBuilder {
    let mut builder = AgentBuilder::from_model_handle(model)
        .name("Craft")
        .default_max_turns(config.max_turns)
        .record_content_telemetry(false);
    if config.preamble.is_empty() {
        builder = builder.without_preamble();
    } else {
        builder = builder.preamble(&config.preamble);
    }
    if let Some(temperature) = config.temperature {
        builder = builder.temperature(temperature);
    }
    if let Some(max_tokens) = config.max_tokens {
        builder = builder.max_tokens(max_tokens);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::{
        agent::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished},
        completion::{Chat, Message, PromptError, Usage},
        test_utils::{MockCompletionModel, MockTurn},
    };

    fn mock_builder(model: &MockCompletionModel, config: &AgentConfig) -> AgentBuilder {
        configure(ModelHandle::named("test-model", model.clone()), config)
    }

    #[tokio::test]
    async fn applies_request_settings_and_preserves_rich_response() {
        let usage = Usage {
            input_tokens: 3,
            output_tokens: 2,
            total_tokens: 5,
            ..Usage::new()
        };
        let model = MockCompletionModel::new([MockTurn::text("hello").with_usage(usage)]);
        let config = AgentConfig {
            preamble: "Custom instructions".into(),
            temperature: Some(0.4),
            max_tokens: Some(128),
            ..AgentConfig::default()
        };
        let agent = mock_builder(&model, &config).build();
        let response = agent.runner("hi").run().await.unwrap();
        assert_eq!(response.output(), "hello");
        assert_eq!(response.usage(), usage);
        assert_eq!(response.requests(), 1);
        assert_eq!(response.messages().unwrap().len(), 2);
        assert_eq!(agent.model_handle().label(), Some("test-model"));
        let request = &model.requests()[0];
        assert_eq!(
            request.chat_history[0],
            Message::system("Custom instructions")
        );
        assert_eq!(request.temperature, Some(0.4));
        assert_eq!(request.max_tokens, Some(128));
        assert!(request.tools.is_empty());
    }

    #[tokio::test]
    async fn defaults_leave_sampling_and_output_limits_to_the_model() {
        let model = MockCompletionModel::text("hello");
        let agent = mock_builder(&model, &AgentConfig::default()).build();
        agent.runner("hi").run().await.unwrap();
        let request = &model.requests()[0];
        assert_eq!(
            request.chat_history[0],
            Message::system(AgentConfig::default().preamble)
        );
        assert_eq!(request.temperature, None);
        assert_eq!(request.max_tokens, None);
    }

    #[tokio::test]
    async fn native_chat_replays_history_and_does_not_commit_failed_runs() {
        let model = MockCompletionModel::new([
            MockTurn::text("first answer"),
            MockTurn::text("second answer"),
            MockTurn::error("provider unavailable"),
        ]);
        let config = AgentConfig {
            preamble: String::new(),
            ..AgentConfig::default()
        };
        let agent = mock_builder(&model, &config).build();
        let mut history = Vec::new();
        assert_eq!(
            agent.chat("first question", &mut history).await.unwrap(),
            "first answer"
        );
        assert_eq!(history.len(), 2);
        let first_turn = history.clone();
        agent.chat("second question", &mut history).await.unwrap();
        assert_eq!(history.len(), 4);
        let request = &model.requests()[1];
        assert_eq!(&request.chat_history[..2], first_turn.as_slice());
        assert!(request.preamble.is_none());
        assert!(
            !request
                .chat_history
                .iter()
                .any(|message| matches!(message, Message::System { .. }))
        );
        let committed = history.clone();
        assert!(matches!(
            agent.chat("third question", &mut history).await,
            Err(PromptError::CompletionError(_))
        ));
        assert_eq!(history, committed);
    }

    struct Retry;

    impl AgentHook for Retry {
        async fn on_model_turn_finished(
            &self,
            _: &HookContext,
            _: ModelTurnFinished<'_>,
        ) -> ModelTurnAction {
            ModelTurnAction::repeat()
        }
    }

    #[tokio::test]
    async fn rig_enforces_the_configured_budget_across_hook_retries() {
        let model =
            MockCompletionModel::new([MockTurn::text("rejected"), MockTurn::text("also rejected")]);
        let config = AgentConfig {
            max_turns: 2,
            ..AgentConfig::default()
        };
        let agent = mock_builder(&model, &config).add_hook(Retry).build();
        let error = agent.runner("hi").run().await.unwrap_err();
        assert!(matches!(
            error,
            PromptError::MaxTurnsError { max_turns: 2, .. }
        ));
        assert_eq!(model.request_count(), 2);
    }

    struct Stop;

    impl AgentHook for Stop {
        async fn on_model_turn_finished(
            &self,
            _: &HookContext,
            _: ModelTurnFinished<'_>,
        ) -> ModelTurnAction {
            ModelTurnAction::stop("cancelled by caller")
        }
    }

    #[tokio::test]
    async fn runner_hooks_can_cancel_without_committing_output() {
        let model = MockCompletionModel::text("not accepted");
        let agent = mock_builder(&model, &AgentConfig::default()).build();
        let result = agent.runner("hi").add_hook(Stop).run().await;
        assert!(matches!(result,
            Err(PromptError::PromptCancelled { reason, .. }) if reason == "cancelled by caller"
        ));
    }

    #[tokio::test]
    async fn no_tools_are_registered_and_unavailable_calls_fail() {
        let model =
            MockCompletionModel::new([MockTurn::tool_call("call-1", "shell", Default::default())]);
        let agent = mock_builder(&model, &AgentConfig::default()).build();
        let mut history = vec![Message::user("previous context")];
        let original = history.clone();
        let result = agent.chat("run a command", &mut history).await;
        assert!(matches!(result,
            Err(PromptError::UnknownToolCall { tool_name, .. }) if tool_name == "shell"
        ));
        assert_eq!(history, original);
        assert_eq!(model.request_count(), 1);
    }

    #[test]
    fn rejects_invalid_selection_and_programmatic_config() {
        let provider = Provider::Llamafile(
            rig::providers::llamafile::Client::from_url("http://127.0.0.1:1").unwrap(),
        );
        assert!(builder(&provider, " ", &AgentConfig::default()).is_err());
        let config = AgentConfig {
            max_turns: 0,
            ..AgentConfig::default()
        };
        assert!(builder(&provider, "test", &config).is_err());
        let provider = Provider::Voyageai(
            rig::providers::voyageai::Client::builder()
                .api_key("test-key")
                .build()
                .unwrap(),
        );
        let error = builder(&provider, "embedding-model", &AgentConfig::default())
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("does not support completion"));
        assert!(format!("{error:#}").contains("voyageai"));
    }
}

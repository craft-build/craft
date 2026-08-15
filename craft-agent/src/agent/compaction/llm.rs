use craft_providers::{ContentBlock, Message, Model, RequestOptions, Role, TokenUsage};
use tracing::info;

use super::super::history::{History, remove_orphaned_tool_results};
use super::super::streaming::stream_with_retry;
use super::strip::{
    strip_images, strip_old_tool_results, strip_thinking, strip_tool_results_by_ratio,
    truncate_oldest_round,
};

use crate::cancel::CancelToken;
use crate::prompt;
use crate::{AgentError, AgentEvent, DoneReason, EventSender, TurnCompleteEvent};

const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If the summary contains a todo list, restore it with todo_write and keep it updated. If you learned important project context during this session, consider saving it to memory before it's lost.";
pub(crate) const MAX_TOKEN_ESTIMATION_MULTIPLIER: f64 = 5.0;
pub(crate) const COMPACT_USER_PROMPT: &str = "What did we do so far?";

fn normalize(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

pub(crate) fn continue_message(config: &craft_config::AgentConfig) -> String {
    match normalize(&config.post_compaction_instructions) {
        Some(extra) => format!("{CONTINUE_AFTER_COMPACT}\n\n{extra}"),
        None => CONTINUE_AFTER_COMPACT.to_string(),
    }
}

const TARGETED_TOPICS_COUNT: usize = 10;
const TARGETED_MIN_SCORE: f32 = 0.5;
const MAX_OVERFLOW_RETRIES: usize = 3;

fn build_compaction_user_message(relevance_scores: Option<&[(usize, f32)]>) -> Message {
    if let Some(scores) = relevance_scores {
        let top_topics: Vec<String> = scores
            .iter()
            .take(TARGETED_TOPICS_COUNT)
            .filter(|(_, score)| *score > TARGETED_MIN_SCORE)
            .map(|(idx, score)| format!("msg#{idx} (relevance: {score:.2})"))
            .collect();
        if !top_topics.is_empty() {
            let topics_str = top_topics.join(", ");
            let prompt = prompt::COMPACTION_TARGETED_USER
                .replace("{topics}", &topics_str)
                .replace("{intent_summary}", "see most recent messages");
            return Message::user(prompt);
        }
    }
    Message::user(COMPACT_USER_PROMPT.to_string())
}

pub(crate) async fn compact_history(
    provider: &dyn craft_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    cancel: &CancelToken,
    relevance_scores: Option<&[(usize, f32)]>,
    config: &craft_config::AgentConfig,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();

    let lifecycle_removed = super::super::read_lifecycle::run_lifecycle(history, None, None).await;
    if lifecycle_removed > 0 {
        info!(
            chars_removed = lifecycle_removed,
            "read lifecycle applied before compaction"
        );
    }

    let mut compaction_history: Vec<Message> = history.as_slice().to_vec();
    remove_orphaned_tool_results(&mut compaction_history);
    strip_images(&mut compaction_history);
    strip_thinking(&mut compaction_history);
    strip_old_tool_results(&mut compaction_history);
    let mut user_message = build_compaction_user_message(relevance_scores);
    if let Some(extra) = normalize(&config.compaction_instructions)
        && let Some(ContentBlock::Text { text }) = user_message.content.iter_mut().next()
    {
        text.push_str("\n\nAdditional instructions:\n");
        text.push_str(extra);
    }
    compaction_history.push(user_message);

    let empty_tools = serde_json::json!([]);
    let mut overflow_retries = 0;
    let mut removal_step = 0;

    let response = loop {
        match stream_with_retry(
            provider,
            model,
            &compaction_history,
            prompt::COMPACTION_SYSTEM,
            &empty_tools,
            event_tx,
            cancel,
            RequestOptions::default(),
            None,
            &[],
            None,
            0,
        )
        .await
        {
            Ok((r, _)) => break r,
            Err(e) if e.is_overflow() => {
                if overflow_retries < MAX_OVERFLOW_RETRIES && compaction_history.len() > 1 {
                    overflow_retries += 1;
                    truncate_oldest_round(&mut compaction_history);
                    info!(
                        attempt = overflow_retries,
                        "truncated oldest round for compaction overflow"
                    );
                    continue;
                }
                if removal_step < super::PROGRESSIVE_TOOL_REMOVAL_RATIOS.len() {
                    let ratio = super::PROGRESSIVE_TOOL_REMOVAL_RATIOS[removal_step];
                    removal_step += 1;
                    let dropped = strip_tool_results_by_ratio(&mut compaction_history, ratio);
                    info!(
                        removal_pct = format!("{:.0}%", ratio * 100.0),
                        dropped, "progressively removed tool responses for compaction overflow"
                    );
                    continue;
                }
                info!(error = %e, "LLM compaction failed, using static fallback");
                return Ok(static_fallback(history));
            }
            Err(e) => {
                info!(error = %e, "LLM compaction failed, using static fallback");
                return Ok(static_fallback(history));
            }
        }
    };

    event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        cost: model.cost_of(&response.usage, false),
        context_size: Some(response.usage.context_tokens()),
        context_window: model.context_window,
    })))?;

    let new_history = vec![Message::user(COMPACT_USER_PROMPT.into()), response.message];
    history.replace(new_history);
    info!(
        model = %model.id,
        duration_ms = compact_start.elapsed().as_millis() as u64,
        "compaction completed"
    );

    Ok(response.usage)
}

pub async fn compact(
    provider: &dyn craft_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    config: &craft_config::AgentConfig,
) -> Result<(), AgentError> {
    let cancel = CancelToken::none();
    let usage = compact_history(provider, model, history, event_tx, &cancel, None, config).await?;
    if let Some(post) = normalize(&config.post_compaction_instructions) {
        history.push(Message::synthetic(post.to_string()));
    }

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        reason: DoneReason::EndTurn,
    })?;

    Ok(())
}

fn static_fallback(history: &mut History) -> TokenUsage {
    let summary = build_static_summary(history.as_slice());
    history.replace(vec![
        Message::user(COMPACT_USER_PROMPT.into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: summary }],
            ..Default::default()
        },
    ]);
    TokenUsage::default()
}

pub(crate) fn build_static_summary(messages: &[Message]) -> String {
    let mut summary = String::from("[Static summary — LLM compaction failed]\n\n");
    let mut user_count = 0;
    let mut tool_names = Vec::new();
    let mut errors = Vec::new();

    for msg in messages {
        if matches!(msg.role, Role::User) {
            user_count += 1;
            let text = msg.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
                _ => None,
            });
            if let Some(text) = text {
                let first_line = text.lines().next().unwrap_or("");
                if !first_line.is_empty() {
                    summary.push_str(&format!("**User**: {first_line}\n"));
                }
            }
        }
        for (_id, name, _input) in msg.tool_uses() {
            tool_names.push(name.to_string());
        }
        for block in &msg.content {
            if let ContentBlock::ToolResult { content, .. } = block
                && (content.contains("error") || content.contains("Error"))
            {
                let snippet: String = content.chars().take(super::ERROR_SNIPPET_CHARS).collect();
                errors.push(snippet);
            }
        }
    }

    if !tool_names.is_empty() {
        summary.push_str(&format!("\n**Tools used**: {}\n", tool_names.join(", ")));
    }
    if !errors.is_empty() {
        summary.push_str("\n**Errors encountered**:\n");
        for e in &errors {
            summary.push_str(&format!("- {e}\n"));
        }
    }
    summary.push_str(&format!("\n**Total user messages**: {user_count}\n"));

    summary
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        IMAGE_PLACEHOLDER, MockProvider, OverflowProvider, default_model, small_context_model,
        text_response, tool_result_block, tool_use_msg,
    };
    use super::*;
    use craft_providers::{ContentBlock, Message, Role, StopReason, TokenUsage};
    use std::sync::{Arc, Mutex};
    use test_case::test_case;

    #[tokio::test]
    async fn compact_replaces_history_with_summary() {
        let provider: Arc<dyn craft_providers::provider::Provider> = Arc::new(MockProvider::new(
            vec![Ok(text_response(StopReason::EndTurn))],
        ));
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
        ]);

        compact(
            &*provider,
            &model,
            &mut history,
            &EventSender::new(raw_tx, 0),
            &craft_config::AgentConfig::default(),
        )
        .await
        .unwrap();

        let msgs = history.as_slice();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));
    }

    #[tokio::test]
    async fn compact_applies_custom_instructions() {
        const EXTRA: &str = "Record anything that belongs in plan.md";
        const POST: &str = "Re-read plan.md and agent.md";

        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![Message::user("work".into())]);
        let (raw_tx, _rx) = flume::unbounded();
        let config = craft_config::AgentConfig {
            compaction_instructions: Some(EXTRA.into()),
            post_compaction_instructions: Some(POST.into()),
            ..Default::default()
        };

        compact(
            &provider,
            &default_model(),
            &mut history,
            &EventSender::new(raw_tx, 0),
            &config,
        )
        .await
        .unwrap();

        let requests = provider.requests.lock().unwrap();
        let summary_prompt = requests[0].last().unwrap();
        assert!(
            matches!(&summary_prompt.content[0], ContentBlock::Text { text }
                if text.ends_with(EXTRA))
        );
        assert!(matches!(&history.as_slice().last().unwrap().content[0],
            ContentBlock::Text { text } if text == POST));
    }

    #[test_case(Some("  \n ".into()), None ; "whitespace_only_is_none")]
    #[test_case(Some("  keep plan.md ".into()), Some("keep plan.md") ; "trimmed")]
    fn normalize_instructions(raw: Option<String>, expected: Option<&str>) {
        assert_eq!(normalize(&raw), expected);
    }

    #[tokio::test]
    async fn compact_preparation_removes_orphan_result_and_tool_image() {
        use craft_providers::{ImageMediaType, ImageSource};

        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let image = ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from("aGVsbG8=")),
        };
        let mut orphan = Message {
            role: Role::User,
            content: vec![tool_result_block("orphan"), image.clone()],
            ..Default::default()
        };
        orphan.content.push(ContentBlock::Text {
            text: "keep text".into(),
        });
        let chat_image = Message {
            role: Role::User,
            content: vec![image],
            ..Default::default()
        };
        let mut history = History::new(vec![orphan, chat_image]);
        let (raw_tx, _rx) = flume::unbounded();

        compact_history(
            &provider,
            &default_model(),
            &mut history,
            &EventSender::new(raw_tx, 0),
            &CancelToken::none(),
            None,
            &craft_config::AgentConfig::default(),
        )
        .await
        .unwrap();

        let requests = provider.requests.lock().unwrap();
        let request = &requests[0];
        assert!(
            !request
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
                ))
        );
        assert!(
            request
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::Text { text } if text == "keep text"))
        );
        assert!(request.iter().flat_map(|message| &message.content).any(
            |block| matches!(block, ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        ));
    }

    #[tokio::test]
    async fn compact_history_retries_without_reproducing_orphan() {
        const TOOL_USE_ID: &str = "call_dMZDTpEfz2JxMvFbqFHua1Zy";

        let provider = MockProvider::new(vec![
            Err(AgentError::ContextOverflow {
                message: "prompt is too long".into(),
            }),
            Ok(text_response(StopReason::EndTurn)),
        ]);
        let mut history = History::new(vec![
            Message::user("request".into()),
            tool_use_msg(TOOL_USE_ID),
            Message {
                role: Role::User,
                content: vec![tool_result_block(TOOL_USE_ID)],
                ..Default::default()
            },
            Message::user("prompt".into()),
        ]);
        let (raw_tx, _rx) = flume::unbounded();

        compact_history(
            &provider,
            &default_model(),
            &mut history,
            &EventSender::new(raw_tx, 0),
            &CancelToken::none(),
            None,
            &craft_config::AgentConfig::default(),
        )
        .await
        .unwrap();

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(
                    block,
                    ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == TOOL_USE_ID
                ))
        );
        assert!(
            !requests[1]
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        );
    }

    #[tokio::test]
    async fn compaction_keeps_observation_before_dependent_reply() {
        let provider = MockProvider::new(vec![Ok(text_response(StopReason::EndTurn))]);
        let mut history = History::new(vec![
            Message::observation("[monitor] build failed".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will fix it".into(),
                }],
                ..Default::default()
            },
        ]);
        let (raw_tx, _rx) = flume::unbounded();

        compact_history(
            &provider,
            &default_model(),
            &mut history,
            &EventSender::new(raw_tx, 0),
            &CancelToken::none(),
            None,
            &craft_config::AgentConfig::default(),
        )
        .await
        .unwrap();

        let requests = provider.requests.lock().unwrap();
        assert!(requests[0][0].is_observation());
        assert!(matches!(requests[0][1].role, Role::Assistant));
    }

    #[tokio::test]
    async fn compact_history_recovers_from_overflow_via_progressive_removal() {
        let provider: Arc<dyn craft_providers::provider::Provider> = Arc::new(OverflowProvider {
            overflows_left: Mutex::new(5),
        });
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "cat huge"}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "x".repeat(500),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ]);

        compact_history(
            &*provider,
            &model,
            &mut history,
            &EventSender::new(raw_tx, 0),
            &CancelToken::none(),
            None,
            &craft_config::AgentConfig::default(),
        )
        .await
        .unwrap();

        let msgs = history.as_slice();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));
    }

    #[test_case(159_999, 0,       0,       0,      200_000, false ; "below_threshold")]
    #[test_case(160_000, 0,       0,       0,      200_000, true  ; "at_threshold")]
    #[test_case(100,     0,       0,       0,      100,     true  ; "tiny_context_window")]
    #[test_case(5_000,   165_000, 10_000,  0,      200_000, true  ; "cached_tokens_count_toward_overflow")]
    #[test_case(100_000, 0,       0,       80_000, 200_000, true  ; "output_tokens_count_toward_overflow")]
    #[test_case(262_144, 0,       0,       0,      262_144, true  ; "equal_context_and_max_output")]
    #[test_case(51_199,  0,       0,       0,      64_000,  false ; "small_window_below_scaled_threshold")]
    #[test_case(51_200,  0,       0,       0,      64_000,  true  ; "small_window_at_scaled_threshold")]
    fn overflow_detection(
        input: u32,
        cache_read: u32,
        cache_creation: u32,
        output: u32,
        ctx_window: u32,
        expected: bool,
    ) {
        let model = small_context_model(ctx_window);
        let usage = TokenUsage {
            input,
            output,
            cache_read,
            cache_creation,
        };
        let buffer = crate::AgentConfig::default()
            .compaction_buffer
            .resolve(model.context_window);
        assert_eq!(super::super::is_overflow(&usage, &model, buffer), expected);
    }

    #[test_case(craft_config::CompactionBuffer::Tokens(10_000), 53_999, false ; "explicit_tokens_below")]
    #[test_case(craft_config::CompactionBuffer::Tokens(10_000), 54_000, true  ; "explicit_tokens_honored")]
    #[test_case(craft_config::CompactionBuffer::Percent(50),    32_000, true  ; "explicit_percent_at_threshold")]
    fn overflow_with_explicit_buffer(
        buffer: craft_config::CompactionBuffer,
        input: u32,
        expected: bool,
    ) {
        let model = small_context_model(64_000);
        let usage = TokenUsage {
            input,
            ..Default::default()
        };
        assert_eq!(
            super::super::is_overflow(&usage, &model, buffer.resolve(model.context_window)),
            expected
        );
    }
}

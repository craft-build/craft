use std::sync::Arc;

use craft_providers::adapt_images_for_model;
use craft_providers::provider::Provider;
use craft_providers::retry::{MAX_TIMEOUT_RETRIES, RetryState};
use craft_providers::roles::ChainHop;
use craft_providers::{
    ContentBlock, Message, Model, ProviderEvent, RequestOptions, StreamResponse,
};
use craft_storage::id::SessionRef;
use serde_json::Value;
use tracing::warn;

use crate::agent::ttsr::TtsrManager;
use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender};

const FUNCTIONS_PREFIX: &str = "functions.";

/// GPT models sometimes emit `functions.<name>`, a Codex training habit.
/// Stripped here at the provider boundary so no raw name enters the agent;
/// `tool_dispatch::run` reuses this for names arriving out of model JSON.
pub(crate) fn canonical_tool_name(name: &str) -> &str {
    name.strip_prefix(FUNCTIONS_PREFIX).unwrap_or(name)
}

fn canonicalize_tool_names(message: &mut Message) {
    for block in &mut message.content {
        if let ContentBlock::ToolUse { name, .. } = block {
            *name = canonical_tool_name(name).to_owned();
        }
    }
}

async fn forward_provider_events(
    prx: flume::Receiver<ProviderEvent>,
    event_tx: &EventSender,
    ttsr: Option<Arc<TtsrManager>>,
    turn: u32,
    fired: Arc<std::sync::Mutex<Option<String>>>,
) -> String {
    let mut streamed = String::new();
    let ttsr_ref = ttsr.as_deref();
    while let Ok(pe) = prx.recv_async().await {
        let ae = match &pe {
            ProviderEvent::TextDelta { text } => {
                observe_ttsr(ttsr_ref, text, turn, &fired);
                streamed.push_str(text);
                AgentEvent::TextDelta { text: text.clone() }
            }
            ProviderEvent::ThinkingDelta { text } => {
                observe_ttsr(ttsr_ref, text, turn, &fired);
                AgentEvent::ThinkingDelta { text: text.clone() }
            }
            ProviderEvent::ToolUseStart { id, name } => AgentEvent::ToolPending {
                id: id.clone(),
                name: canonical_tool_name(name).to_owned(),
            },
            ProviderEvent::PromptProgress {
                processed,
                total,
                cache,
            } => AgentEvent::PromptProgress {
                processed: *processed,
                total: *total,
                cache: *cache,
            },
        };
        if event_tx.send(ae).is_err() {
            break;
        }
    }
    streamed
}

fn observe_ttsr(
    ttsr: Option<&TtsrManager>,
    delta: &str,
    turn: u32,
    fired: &Arc<std::sync::Mutex<Option<String>>>,
) {
    let Some(t) = ttsr.filter(|t| t.enabled()) else {
        return;
    };
    if fired.lock().unwrap().is_some() {
        return;
    }
    let Some(rule) = t.observe(delta, turn) else {
        return;
    };
    let mut guard = fired.lock().unwrap();
    if guard.is_none() {
        *guard = Some(TtsrManager::injection(rule));
    }
}

/// Cancelling mid-stream carries the text the user still sees on screen,
/// so the caller can keep it in history. A cancel during the retry backoff
/// carries nothing: the `Retry` event already made the view drop the failed
/// attempt's text, and history must agree with the view.
#[derive(Debug)]
pub(crate) enum StreamError {
    Cancelled { streamed: String },
    Other(AgentError),
}

impl From<AgentError> for StreamError {
    fn from(e: AgentError) -> Self {
        Self::Other(e)
    }
}

impl From<StreamError> for AgentError {
    fn from(e: StreamError) -> Self {
        match e {
            StreamError::Cancelled { .. } => Self::Cancelled,
            StreamError::Other(e) => e,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    tools: &Value,
    event_tx: &EventSender,
    cancel: &CancelToken,
    opts: RequestOptions,
    session_id: Option<&SessionRef>,
    fallbacks: &[ChainHop],
    ttsr: Option<Arc<TtsrManager>>,
    turn: u32,
) -> Result<(StreamResponse, Option<String>), StreamError> {
    let mut active_provider: &dyn Provider = provider;
    let mut active_model: &Model = model;
    let messages = adapt_images_for_model(model, messages);
    let messages = &*messages;
    let mut next_fallback = 0usize;
    let mut retry = RetryState::new();
    let mut pending_injection: Option<String> = None;
    loop {
        let cur_provider = active_provider;
        let cur_model = active_model;
        let (ptx, prx) = flume::unbounded();
        let fired = Arc::new(std::sync::Mutex::new(None::<String>));
        let forwarder = tokio::spawn({
            let event_tx = event_tx.clone();
            let fired = Arc::clone(&fired);
            let ttsr = ttsr.clone();
            async move { forward_provider_events(prx, &event_tx, ttsr, turn, fired).await }
        });
        let result = tokio::select! {
            r = cur_provider.stream_message(cur_model, messages, system, tools, &ptx, opts.clamped(cur_model), session_id) => r,
            _ = cancel.cancelled() => Err(AgentError::Cancelled),
        };
        drop(ptx);
        let streamed = forwarder.await.unwrap_or_default();
        if pending_injection.is_none() {
            pending_injection = fired.lock().unwrap().take();
        }
        match result {
            Ok(mut r) => {
                canonicalize_tool_names(&mut r.message);
                return Ok((r, pending_injection));
            }
            Err(AgentError::Cancelled) => return Err(StreamError::Cancelled { streamed }),
            Err(e) if e.is_retryable() => {
                let mut advanced = false;
                if e.should_rotate_key() {
                    let rotated = cur_provider.rotate_key().await.unwrap_or(false);
                    if rotated {
                        warn!("rotated API key after error: {e}");
                        retry = RetryState::new();
                    } else if let Some(hop) = fallbacks.get(next_fallback) {
                        warn!(
                            error = %e,
                            fallback = %hop.model.id,
                            "key rotation exhausted; advancing to fallback chain entry"
                        );
                        active_provider = &*hop.provider;
                        active_model = &hop.model;
                        next_fallback += 1;
                        retry = RetryState::new();
                        advanced = true;
                        event_tx.send(AgentEvent::Retry {
                            attempt: 1,
                            message: format!("failing over to {}", hop.model.id),
                            delay_ms: 0,
                        })?;
                    }
                }
                if !advanced {
                    let (attempt, delay) = retry.next_delay();
                    if matches!(e, AgentError::Timeout { .. }) && attempt > MAX_TIMEOUT_RETRIES {
                        return Err(e.into());
                    }
                    let delay_ms = delay.as_millis() as u64;
                    warn!(attempt, delay_ms, error = %e, "retryable, will retry");
                    event_tx.send(AgentEvent::Retry {
                        attempt,
                        message: e.retry_message(),
                        delay_ms,
                    })?;
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => {}
                    }
                    if cancel.is_cancelled() {
                        return Err(StreamError::Cancelled {
                            streamed: String::new(),
                        });
                    }
                }
            }
            Err(e) if e.should_abort() => return Err(e.into()),
            Err(e) => return Err(e.classify().into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventSender;
    use async_trait::async_trait;
    use craft_providers::{AgentError, ContentBlock, Message, Role, StopReason};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Provider that fails the first N calls with a retryable 429 (no key to rotate),
    /// then succeeds. `fail_forever` makes it always fail (chain exhaustion case).
    struct MockProvider {
        _id: &'static str,
        fail_then_succeed: Arc<AtomicU32>,
        fail_forever: bool,
        seen_model: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn stream_message(
            &self,
            model: &Model,
            _: &[Message],
            _: &str,
            _: &Value,
            _: &flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&craft_storage::id::SessionRef>,
        ) -> Result<StreamResponse, AgentError> {
            let still_failing = self.fail_forever
                || self
                    .fail_then_succeed
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                        if v > 0 { Some(v - 1) } else { None }
                    })
                    .is_ok();
            *self.seen_model.lock().unwrap() = Some(model.id.clone());
            if still_failing {
                return Err(AgentError::Api {
                    status: 429,
                    message: "rate limited".into(),
                });
            }
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                    display_text: None,
                    ..Default::default()
                },
                usage: craft_providers::TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
            })
        }
        async fn list_models(&self) -> Result<Vec<String>, AgentError> {
            Ok(vec![])
        }
        async fn rotate_key(&self) -> Result<bool, AgentError> {
            Ok(false)
        }
    }

    fn hop(id: &'static str, provider: Arc<dyn Provider>) -> ChainHop {
        ChainHop {
            model: Model {
                id: id.into(),
                ..Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
            },
            provider,
        }
    }

    #[tokio::test]
    async fn advances_to_fallback_when_rotation_exhausted() {
        let primary_fail = Arc::new(AtomicU32::new(1));
        let primary_seen = Arc::new(std::sync::Mutex::new(None));
        let primary = MockProvider {
            _id: "primary",
            fail_then_succeed: Arc::clone(&primary_fail),
            fail_forever: false,
            seen_model: Arc::clone(&primary_seen),
        };
        let fallback_seen = Arc::new(std::sync::Mutex::new(None));
        let fallback = MockProvider {
            _id: "fallback",
            fail_then_succeed: Arc::new(AtomicU32::new(0)),
            fail_forever: false,
            seen_model: Arc::clone(&fallback_seen),
        };
        let fallback_hop = hop("fallback-model", Arc::new(fallback));

        let (tx, _rx) = flume::unbounded::<crate::Envelope>();
        let event_tx = EventSender::new(tx, 0);
        let model = Model {
            id: "primary-model".into(),
            ..Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
        };

        let (resp, _injection) = stream_with_retry(
            &primary,
            &model,
            &[],
            "",
            &Value::Array(vec![]),
            &event_tx,
            &crate::cancel::CancelToken::none(),
            RequestOptions::default(),
            None,
            &[fallback_hop],
            None,
            0,
        )
        .await
        .unwrap();

        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(
            primary_seen.lock().unwrap().as_deref(),
            Some("primary-model")
        );
        assert_eq!(
            fallback_seen.lock().unwrap().as_deref(),
            Some("fallback-model")
        );
    }

    #[tokio::test]
    async fn advances_through_all_fallbacks_then_keeps_retrying_last() {
        let primary_seen = Arc::new(std::sync::Mutex::new(None));
        let primary = MockProvider {
            _id: "primary",
            fail_then_succeed: Arc::new(AtomicU32::new(0)),
            fail_forever: true,
            seen_model: Arc::clone(&primary_seen),
        };
        let fallback_seen = Arc::new(std::sync::Mutex::new(None));
        let fallback = MockProvider {
            _id: "fallback",
            fail_then_succeed: Arc::new(AtomicU32::new(0)),
            fail_forever: true,
            seen_model: Arc::clone(&fallback_seen),
        };
        let fallback_hop = hop("fallback-model", Arc::new(fallback));

        let (tx, _rx) = flume::unbounded::<crate::Envelope>();
        let event_tx = EventSender::new(tx, 0);
        let model = Model {
            id: "primary-model".into(),
            ..Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
        };
        let (trigger, cancel) = crate::cancel::CancelToken::new();
        let task = tokio::spawn(async move {
            stream_with_retry(
                &primary,
                &model,
                &[],
                "",
                &Value::Array(vec![]),
                &event_tx,
                &cancel,
                RequestOptions::default(),
                None,
                &[fallback_hop],
                None,
                0,
            )
            .await
        });
        for _ in 0..50 {
            if fallback_seen.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        trigger.cancel();
        let _ = task.await;
        assert_eq!(
            fallback_seen.lock().unwrap().as_deref(),
            Some("fallback-model"),
            "should have advanced to the fallback"
        );
    }

    #[test]
    fn tool_use_names_canonicalized() {
        let mut message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::tool_use("t1", "functions.bash", json!({})),
                ContentBlock::tool_use("t2", "read", json!({})),
                ContentBlock::tool_use("t3", "my_functions.x", json!({})),
            ],
            ..Default::default()
        };
        canonicalize_tool_names(&mut message);
        let names: Vec<&str> = message.tool_uses().map(|(_, name, _)| name).collect();
        assert_eq!(names, ["bash", "read", "my_functions.x"]);
    }
}

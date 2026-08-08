use std::sync::Mutex;

use async_trait::async_trait;
use craft_providers::provider::Provider;
use craft_providers::{
    ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason, StreamResponse,
    TokenUsage,
};
use serde_json::Value;

use crate::AgentError;

pub(super) const IMAGE_PLACEHOLDER: &str = "[image]";

pub(super) struct MockProvider {
    pub responses: Mutex<Vec<Result<StreamResponse, AgentError>>>,
    pub requests: Mutex<Vec<Vec<Message>>>,
}

impl MockProvider {
    pub fn new(responses: Vec<Result<StreamResponse, AgentError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream_message(
        &self,
        _: &Model,
        messages: &[Message],
        _: &str,
        _: &Value,
        _: &flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&craft_storage::id::SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let mut responses = self.responses.lock().unwrap();
        assert!(!responses.is_empty(), "MockProvider: no more responses");
        responses.remove(0)
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        unimplemented!()
    }
}

pub(super) struct OverflowProvider {
    pub overflows_left: Mutex<usize>,
}

#[async_trait]
impl Provider for OverflowProvider {
    async fn stream_message(
        &self,
        _: &Model,
        _: &[Message],
        _: &str,
        _: &Value,
        _: &flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&craft_storage::id::SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        let mut left = self.overflows_left.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            return Err(AgentError::ContextOverflow {
                message: "too long".into(),
            });
        }
        Ok(text_response(StopReason::EndTurn))
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        unimplemented!()
    }
}

pub(super) fn default_model() -> Model {
    Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
}

pub(super) fn small_context_model(context_window: u32) -> Model {
    let mut model = default_model();
    model.context_window = context_window;
    model
}

pub(super) fn text_response(stop_reason: StopReason) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response".into(),
            }],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(stop_reason),
    }
}

pub(super) fn tool_use_msg(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::tool_use(id, "bash", serde_json::json!({}))],
        ..Default::default()
    }
}

pub(super) fn tool_result_block(id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: "output".into(),
        images: vec![],
        is_error: false,
    }
}

#[track_caller]
pub(super) fn assert_tool_results_have_calls(messages: &[Message]) {
    for (index, message) in messages.iter().enumerate() {
        for block in &message.content {
            let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                continue;
            };
            assert!(matches!(message.role, Role::User));
            assert!(index > 0);
            assert!(
                messages[index - 1]
                    .tool_uses()
                    .any(|(id, _, _)| id == tool_use_id)
            );
        }
    }
}

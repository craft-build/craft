//! Translate craft `AgentEvent`s to ACP `SessionUpdate`s.
//!
//! Phase 2 covered text/thinking. Phase 3 adds tool lifecycle (pending/start/
//! output/done) and plan updates from `TodoList` outputs. Permission requests
//! go through `crate::session` because they need access to the connection
//! `cx` to round-trip a request to the client.

use agent_client_protocol::schema::ContentBlock;
use agent_client_protocol::schema::ContentChunk;
use agent_client_protocol::schema::SessionUpdate;
use agent_client_protocol::schema::StopReason;
use agent_client_protocol::schema::ToolCall;
use agent_client_protocol::schema::ToolCallId;
use agent_client_protocol::schema::ToolCallStatus;
use agent_client_protocol::schema::ToolCallUpdate;
use agent_client_protocol::schema::ToolCallUpdateFields;
use craft_agent::AgentEvent;
use craft_agent::ToolDoneEvent;
use craft_agent::ToolOutput;
use craft_agent::ToolStartEvent;
use craft_providers::AgentError;

use crate::plan;
use crate::tool_kinds;

/// Result of translating a single `AgentEvent`. Most events produce zero or
/// one `SessionUpdate`; a `ToolDone` carrying a `TodoList` produces both a
/// completion update and a plan update.
#[derive(Default)]
pub struct Translation {
    pub updates: Vec<SessionUpdate>,
}

impl Translation {
    fn one(update: SessionUpdate) -> Self {
        Self { updates: vec![update] }
    }
}

/// Translate an `AgentEvent` into zero or more `SessionUpdate`s.
pub fn translate(event: &AgentEvent) -> Translation {
    match event {
        AgentEvent::TextDelta { text, .. } => Translation::one(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::from(text.clone())),
        )),
        AgentEvent::ThinkingDelta { text, .. } => Translation::one(
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::from(text.clone()))),
        ),
        AgentEvent::ToolPending { id, name } => {
            Translation::one(SessionUpdate::ToolCall(tool_call_pending(id, name)))
        }
        AgentEvent::ToolStart(start) => {
            Translation::one(SessionUpdate::ToolCallUpdate(tool_call_started(start)))
        }
        AgentEvent::ToolDone(done) => translate_tool_done(done),
        _ => Translation::default(),
    }
}

fn tool_call_pending(id: &str, name: &str) -> ToolCall {
    ToolCall::new(ToolCallId::new(id.to_owned()), name.to_owned())
        .kind(tool_kinds::tool_kind(name))
        .status(ToolCallStatus::Pending)
}

fn tool_call_started(start: &ToolStartEvent) -> ToolCallUpdate {
    let title = if start.summary.is_empty() {
        start.tool.to_string()
    } else {
        start.summary.clone()
    };
    let mut fields = ToolCallUpdateFields::new()
        .status(Some(ToolCallStatus::InProgress))
        .title(Some(title))
        .kind(Some(tool_kinds::tool_kind(&start.tool)));
    if let Some(raw) = start.raw_input.clone() {
        fields = fields.raw_input(Some(raw));
    }
    ToolCallUpdate::new(ToolCallId::new(start.id.clone()), fields)
}

fn translate_tool_done(done: &ToolDoneEvent) -> Translation {
    let status = if done.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    let content = tool_kinds::render_content(&done.output);
    let fields = ToolCallUpdateFields::new()
        .status(Some(status))
        .content(Some(content));
    let mut updates = vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new(done.id.clone()),
        fields,
    ))];
    if let ToolOutput::TodoList(items) = &done.output {
        updates.push(SessionUpdate::Plan(plan::build_plan(items)));
    }
    Translation { updates }
}

/// Map a craft run result to an ACP `StopReason`.
pub fn stop_reason(result: &Result<(), AgentError>) -> StopReason {
    match result {
        Ok(()) => StopReason::EndTurn,
        Err(AgentError::Cancelled) => StopReason::Cancelled,
        Err(AgentError::ContextOverflow { .. }) => StopReason::MaxTokens,
        Err(_) => StopReason::Refusal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_agent::TodoItem;
    use craft_agent::TodoPriority;
    use craft_agent::TodoStatus;
    use std::sync::Arc;
    use test_case::test_case;

    fn done(output: ToolOutput, is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            id: "tu_1".into(),
            tool: Arc::from("read"),
            output,
            is_error,
        }
    }

    #[test]
    fn translates_text_delta() {
        let evt = AgentEvent::TextDelta { text: "hi".into() };
        let out = translate(&evt);
        assert_eq!(out.updates.len(), 1);
        assert!(matches!(out.updates[0], SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn translates_thinking_delta() {
        let evt = AgentEvent::ThinkingDelta { text: "...".into() };
        let out = translate(&evt);
        assert!(matches!(out.updates[0], SessionUpdate::AgentThoughtChunk(_)));
    }

    #[test]
    fn translates_tool_pending() {
        let evt = AgentEvent::ToolPending {
            id: "tu_1".into(),
            name: "read".into(),
        };
        let out = translate(&evt);
        assert_eq!(out.updates.len(), 1);
        match &out.updates[0] {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(&*tc.tool_call_id.0, "tu_1");
                assert_eq!(tc.title, "read");
                assert_eq!(tc.status, ToolCallStatus::Pending);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn translates_tool_done_completed() {
        let evt = AgentEvent::ToolDone(Box::new(done(ToolOutput::Plain("ok".into()), false)));
        let out = translate(&evt);
        assert_eq!(out.updates.len(), 1);
        match &out.updates[0] {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::Completed));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn translates_tool_done_failed() {
        let evt = AgentEvent::ToolDone(Box::new(done(ToolOutput::Plain("boom".into()), true)));
        let out = translate(&evt);
        match &out.updates[0] {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::Failed));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn todolist_done_emits_plan_update_too() {
        let items = vec![TodoItem {
            content: "task".into(),
            status: TodoStatus::Pending,
            priority: TodoPriority::Medium,
        }];
        let evt = AgentEvent::ToolDone(Box::new(done(ToolOutput::TodoList(items), false)));
        let out = translate(&evt);
        assert_eq!(out.updates.len(), 2);
        assert!(matches!(out.updates[0], SessionUpdate::ToolCallUpdate(_)));
        assert!(matches!(out.updates[1], SessionUpdate::Plan(_)));
    }

    #[test]
    fn diff_done_renders_diff_content() {
        use agent_client_protocol::schema::ToolCallContent;
        let output = ToolOutput::Diff {
            path: "x.rs".into(),
            before: "a".into(),
            after: "b".into(),
            summary: "+1 -1".into(),
        };
        let evt = AgentEvent::ToolDone(Box::new(done(output, false)));
        let out = translate(&evt);
        match &out.updates[0] {
            SessionUpdate::ToolCallUpdate(u) => {
                let content = u.fields.content.as_ref().expect("content present");
                assert_eq!(content.len(), 1);
                assert!(matches!(content[0], ToolCallContent::Diff(_)));
            }
            other => panic!("expected ToolCallUpdate, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_event_produces_no_updates() {
        let evt = AgentEvent::Info { message: "x".into() };
        assert!(translate(&evt).updates.is_empty());
    }

    #[test_case(Ok(()), StopReason::EndTurn ; "ok maps to end_turn")]
    #[test_case(Err(AgentError::Cancelled), StopReason::Cancelled ; "cancelled")]
    #[test_case(
        Err(AgentError::ContextOverflow { message: "x".into() }),
        StopReason::MaxTokens
        ; "overflow"
    )]
    #[test_case(Err(AgentError::Channel), StopReason::Refusal ; "channel error")]
    fn stop_reason_mapping(result: Result<(), AgentError>, expected: StopReason) {
        assert_eq!(stop_reason(&result), expected);
    }
}

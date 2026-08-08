use craft_providers::{ContentBlock, Message, Model, Role};

use super::super::history::History;

use super::context_under_limit;

use crate::AgentError;

const VCC_SUMMARY_PREFIX: &str = "This summary captures";

pub(super) fn is_vcc_summary(msg: &Message) -> bool {
    matches!(msg.role, Role::Assistant)
        && msg.content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text.starts_with(VCC_SUMMARY_PREFIX)),
        )
}

pub(super) fn summary_text(msg: &Message) -> Option<&str> {
    msg.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

/// No-LLM VCC compaction: summarize the head, keep the tail.
/// Returns Ok(true) if under the limit, Ok(false) to fall back to the LLM compactor.
pub(crate) fn vcc_compact(
    history: &mut History,
    model: &Model,
    compaction_buffer: u32,
    multiplier: f64,
) -> Result<bool, AgentError> {
    let messages = history.as_slice();
    let (prev, live_start) = match messages.first() {
        Some(m) if is_vcc_summary(m) => (summary_text(m), 1),
        _ => (None, 0),
    };
    let live = &messages[live_start..];
    if live.len() <= 2 {
        return Ok(false);
    }
    let super::vcc::VccSummary {
        summary,
        tail_start,
    } = super::vcc::compact(live, prev);
    if summary.is_empty() {
        return Ok(false);
    }
    let tail_start = tail_start.min(live.len());
    let tail_msgs = live.len() - tail_start;
    let tail: Vec<Message> = live[tail_start..].to_vec();
    let mut new_history = Vec::with_capacity(1 + tail.len());
    new_history.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: summary }],
        ..Default::default()
    });
    new_history.extend(tail);
    history.replace(new_history);
    let under = context_under_limit(history, model, compaction_buffer, multiplier);
    tracing::info!(tail_msgs, under_limit = under, "vcc compaction applied");
    Ok(under)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::default_model;
    use super::*;

    fn vcc_history() -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(Message::user(format!("do task {i}")));
            msgs.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    format!("t{i}"),
                    "bash",
                    serde_json::json!({"command": format!("echo step{i}")}),
                )],
                ..Default::default()
            });
            msgs.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: format!("output {i}"),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            });
        }
        msgs.push(Message::user("final user message".into()));
        msgs
    }

    #[tokio::test]
    async fn vcc_compact_succeeds_without_llm_and_keeps_tail() {
        let mut history = History::new(vcc_history());
        let model = default_model();
        let buffer = crate::AgentConfig::default()
            .compaction_buffer
            .resolve(model.context_window);
        let under = vcc_compact(&mut history, &model, buffer, 1.0).unwrap();
        assert!(under, "vcc should bring context under the limit");
        let msgs = history.as_slice();
        assert!(matches!(msgs[0].role, Role::Assistant));
        assert!(is_vcc_summary(&msgs[0]));
        assert!(msgs.len() > 1, "tail must be preserved");
        assert!(matches!(msgs[1].role, Role::User));
    }

    #[tokio::test]
    async fn vcc_compact_falls_back_when_still_over_limit() {
        let mut history = History::new(vcc_history());
        let model = super::super::test_support::small_context_model(50);
        let under = vcc_compact(&mut history, &model, 10, 1.0).unwrap();
        assert!(!under, "tiny context window should remain over the limit");
    }

    #[tokio::test]
    async fn vcc_compact_multiplier_makes_fall_back_earlier() {
        let compaction_buffer = 0;

        let mut probe = History::new(vcc_history());
        let probe_model = super::super::test_support::small_context_model(1_000_000);
        assert!(vcc_compact(&mut probe, &probe_model, compaction_buffer, 1.0).unwrap());
        let estimated = probe.estimate_tokens(&probe_model);
        assert!(estimated > 0);

        let context_window = estimated + estimated / 2;

        let mut history = History::new(vcc_history());
        let model = super::super::test_support::small_context_model(context_window);
        assert!(
            vcc_compact(&mut history, &model, compaction_buffer, 1.0).unwrap(),
            "multiplier=1.0 should be under the limit"
        );

        let mut history = History::new(vcc_history());
        assert!(
            !vcc_compact(&mut history, &model, compaction_buffer, 2.0).unwrap(),
            "multiplier=2.0 should be over the limit"
        );
    }
}

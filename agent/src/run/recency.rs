//! Recency tail: volatile per-turn facts (turn counters and similar) rendered
//! as a `<turn-context>` block appended to the last user message of the
//! request only. The tail never enters the system prompt (which would break
//! prompt-cache stability) and is never committed to history; it is rebuilt
//! from scratch every request and discarded after.

use std::sync::Arc;

use crate::history::{Message, UserContent};

const RECENCY_HEADER: &str = "<turn-context>";

/// A small, per-turn bundle of volatile facts.
#[derive(Debug, Clone, Default)]
pub struct RecencyFacts {
    blocks: Vec<String>,
}

impl RecencyFacts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, block: String) {
        if !block.is_empty() {
            self.blocks.push(block);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Render the blocks under a single header; empty when there is nothing
    /// to inject, so an empty [`RecencyFacts`] is a no-op for the request.
    pub fn render(&self) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(64);
        out.push_str(RECENCY_HEADER);
        out.push_str("\n\n");
        out.push_str(&self.blocks.join("\n\n"));
        out
    }
}

/// Handed to a [`RecencySource`] each request. Carries the minimal signal a
/// volatile source needs today; extend it only when a real source requires
/// more, keeping the per-turn surface small.
#[derive(Debug, Clone, Copy)]
pub struct RecencyCtx {
    pub turn: u32,
}

/// Extension point for per-turn volatile facts. The host surface (TUI, ACP,
/// a future plugin runtime) supplies one concrete implementation; the run
/// holds it as `Option<Arc<dyn RecencySource>>` and consults it once per
/// request.
pub trait RecencySource: Send + Sync {
    fn collect(&self, ctx: &RecencyCtx) -> RecencyFacts;
}

/// Return a copy of `messages` with the rendered tail appended to the last
/// user message, or `None` when there is nothing to inject or no user
/// message to carry it. The input is never modified.
pub fn attach_recency_tail(messages: &[Message], facts: &RecencyFacts) -> Option<Vec<Message>> {
    if facts.is_empty() {
        return None;
    }
    let tail = facts.render();
    let last_user = messages
        .iter()
        .rposition(|message| matches!(message, Message::User { .. }))?;
    let mut out = messages.to_vec();
    let Message::User { content } = &mut out[last_user] else {
        return None;
    };
    content.push(UserContent::text(tail));
    Some(out)
}

/// Collect this turn's facts and attach them, if any.
pub(crate) fn recency_view(
    source: &Arc<dyn RecencySource>,
    messages: &[Message],
    turn: u32,
) -> Option<Vec<Message>> {
    let facts = source.collect(&RecencyCtx { turn });
    attach_recency_tail(messages, &facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::UserContent;

    fn block_text(block: &UserContent) -> String {
        match block {
            UserContent::Text(text) => text.text.clone(),
            UserContent::ToolResult(result) => result
                .content
                .first()
                .map(|item| item.to_text())
                .unwrap_or_default(),
        }
    }

    #[test]
    fn empty_facts_are_a_noop() {
        let messages = vec![Message::user("hello")];
        assert!(attach_recency_tail(&messages, &RecencyFacts::new()).is_none());
    }

    #[test]
    fn tail_lands_only_on_last_user_message() {
        let messages = vec![Message::user("first"), Message::user("latest")];
        let mut facts = RecencyFacts::new();
        facts.push("fresh state".into());

        let out = attach_recency_tail(&messages, &facts).expect("non-empty facts attach");
        assert_eq!(out.len(), messages.len());
        let Message::User { content } = &out[0] else {
            panic!("user message");
        };
        assert_eq!(content.len(), 1, "earlier user message untouched");
        let Message::User { content } = &out[1] else {
            panic!("user message");
        };
        assert_eq!(content.len(), 2);
        assert!(block_text(&content[1]).starts_with("<turn-context>"));
        assert!(block_text(&content[1]).contains("fresh state"));
        // Input untouched.
        let Message::User { content } = &messages[1] else {
            panic!("user message");
        };
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn no_user_message_is_a_noop() {
        let messages = vec![Message::system("sys")];
        let mut facts = RecencyFacts::new();
        facts.push("x".into());
        assert!(attach_recency_tail(&messages, &facts).is_none());
    }

    #[test]
    fn blank_blocks_are_dropped() {
        let mut facts = RecencyFacts::new();
        facts.push(String::new());
        assert!(facts.is_empty());
        assert_eq!(facts.render(), "");
    }

    struct FixedSource;
    impl RecencySource for FixedSource {
        fn collect(&self, ctx: &RecencyCtx) -> RecencyFacts {
            let mut facts = RecencyFacts::new();
            facts.push(format!("turn {}", ctx.turn));
            facts
        }
    }

    #[test]
    fn recency_view_carries_turn_context() {
        let source: Arc<dyn RecencySource> = Arc::new(FixedSource);
        let messages = vec![Message::user("go")];
        let out = recency_view(&source, &messages, 7).expect("facts attached");
        let Message::User { content } = &out[0] else {
            panic!("user message");
        };
        assert!(block_text(&content[1]).contains("turn 7"));
    }
}

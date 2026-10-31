//! Recency tail: volatile per-turn facts (turn counters and similar) rendered
//! as a `<turn-context>` block appended to the last user message of the
//! request only. The tail never enters the system prompt (which would break
//! prompt-cache stability) and is never committed to history; it is rebuilt
//! from scratch every request and discarded after.

use std::sync::Arc;

use crate::history::{ContentBlock, StoredMessage, ext};

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
pub fn attach_recency_tail(
    messages: &[StoredMessage],
    facts: &RecencyFacts,
) -> Option<Vec<StoredMessage>> {
    if facts.is_empty() {
        return None;
    }
    let tail = facts.render();
    let last_user = messages
        .iter()
        .rposition(|stored| ext::is_user(stored.message()))?;
    let mut out = messages.to_vec();
    out[last_user]
        .message_mut()
        .content
        .push(ContentBlock::Text { text: tail });
    Some(out)
}

/// Collect this turn's facts and attach them, if any.
pub(crate) fn recency_view(
    source: &Arc<dyn RecencySource>,
    messages: &[StoredMessage],
    turn: u32,
) -> Option<Vec<StoredMessage>> {
    let facts = source.collect(&RecencyCtx { turn });
    attach_recency_tail(messages, &facts)
}

/// Attach the rendered tail to the last user message of a wire view, in
/// place; a no-op when there is nothing to inject or no user message to
/// carry it.
pub(crate) fn attach_recency_tail_wire(
    messages: &mut [crate::history::Message],
    source: &Arc<dyn RecencySource>,
    turn: u32,
) {
    let facts = source.collect(&RecencyCtx { turn });
    if facts.is_empty() {
        return;
    }
    let tail = facts.render();
    let Some(last_user) = messages.iter().rposition(ext::is_user) else {
        return;
    };
    messages[last_user]
        .content
        .push(ContentBlock::Text { text: tail });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_text(block: &ContentBlock) -> String {
        match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Image { .. } => "[image]".to_owned(),
            ContentBlock::ToolResult { content, .. } => content
                .0
                .first()
                .map(|item| match item {
                    crate::history::ToolContent::Text { text } => text.clone(),
                    crate::history::ToolContent::Image { .. } => "[image]".to_owned(),
                })
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    #[test]
    fn empty_facts_are_a_noop() {
        let messages = vec![ext::stored_user_text("hello")];
        assert!(attach_recency_tail(&messages, &RecencyFacts::new()).is_none());
    }

    #[test]
    fn tail_lands_only_on_last_user_message() {
        let messages = vec![
            ext::stored_user_text("first"),
            ext::stored_user_text("latest"),
        ];
        let mut facts = RecencyFacts::new();
        facts.push("fresh state".into());

        let out = attach_recency_tail(&messages, &facts).expect("non-empty facts attach");
        assert_eq!(out.len(), messages.len());
        assert_eq!(
            out[0].message().content.len(),
            1,
            "earlier user message untouched"
        );
        assert_eq!(out[1].message().content.len(), 2);
        assert!(block_text(&out[1].message().content[1]).starts_with("<turn-context>"));
        assert!(block_text(&out[1].message().content[1]).contains("fresh state"));
        assert_eq!(messages[1].message().content.len(), 1);
    }

    #[test]
    fn no_user_message_is_a_noop() {
        let messages = vec![ext::stored(ext::system_text("sys"))];
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
        let messages = vec![ext::stored_user_text("go")];
        let out = recency_view(&source, &messages, 7).expect("facts attached");
        assert!(block_text(&out[0].message().content[1]).contains("turn 7"));
    }
}

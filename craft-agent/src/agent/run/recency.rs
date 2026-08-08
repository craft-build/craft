use std::sync::Arc;

use craft_providers::{Message, Role};
use tracing::info;

use super::Agent;

const MANDATORY_RECENT_MESSAGES: usize = 6;

pub(super) struct AgentRecency {
    pub(super) scorer: Option<crate::agent::semantic::RelevanceScorer>,
    pub(super) recency_source: Option<Arc<dyn crate::prompt::RecencySource>>,
    pub(super) repo_map: Option<craft_repomap::RepoMap>,
}

fn append_recency_tail(
    messages: &[Message],
    facts: &crate::prompt::RecencyFacts,
) -> Option<Vec<Message>> {
    if facts.is_empty() {
        return None;
    }
    let tail = facts.render();
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User))?;
    let mut out = messages.to_vec();
    out[last_user]
        .content
        .push(craft_providers::ContentBlock::Text { text: tail });
    Some(out)
}

impl<'h> Agent<'h> {
    /// Collect volatile facts for this turn and, if any, return a copy of
    /// `messages` with the rendered tail appended to the last user message.
    /// Returns `None` when there is no source or nothing to inject, leaving
    /// the request byte-identical to today. `self.history` is never touched.
    pub(super) fn attach_recency_tail(&self, messages: &[Message]) -> Option<Vec<Message>> {
        let source = self.recency.recency_source.as_ref()?;
        let facts = source.collect(&crate::prompt::RecencyCtx {
            turn: self.num_turns,
        });
        append_recency_tail(messages, &facts)
    }

    pub(super) async fn build_intent(&self) -> Option<Vec<f32>> {
        let scorer = self.recency.scorer.as_ref()?;
        scorer.build_intent(self.history.as_slice()).await.ok()
    }

    pub(super) async fn build_semantic_view(&self, intent: &[f32]) -> Option<Vec<Message>> {
        let scorer = self.recency.scorer.as_ref()?;
        let scores = scorer
            .score_messages(self.history.as_slice(), intent)
            .await
            .ok()?;
        let token_budget = self.io.model.context_window.saturating_sub(
            self.config
                .resolve_compaction_buffer(self.io.model.context_window),
        );
        let selected = crate::agent::semantic::select_messages(
            &scores,
            self.history.len(),
            token_budget,
            MANDATORY_RECENT_MESSAGES,
            self.compaction.cache_tracker.frozen_count(),
            &|idx| self.history.message_token_estimate(&self.io.model, idx),
        );
        if selected.len() < self.history.len() {
            info!(
                total = self.history.len(),
                selected = selected.len(),
                "semantic context curation applied"
            );
            Some(self.history.select_view(&selected, self.history.len()))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_providers::{ContentBlock, Message, Role};

    #[test]
    fn append_recency_tail_empty_is_noop() {
        let messages = vec![
            Message::user("hello".into()),
            Message::observation("obs".into()),
        ];
        let facts = crate::prompt::RecencyFacts::new();
        assert!(append_recency_tail(&messages, &facts).is_none());
    }

    #[test]
    fn append_recency_tail_lands_only_on_last_user_message() {
        let messages = vec![
            Message::user("first".into()),
            Message::observation("middle".into()),
            Message::user("latest".into()),
        ];
        let mut facts = crate::prompt::RecencyFacts::new();
        facts.push("fresh state".into());

        let out = append_recency_tail(&messages, &facts).expect("non-empty facts attach");
        assert_eq!(out.len(), messages.len());

        assert_eq!(out[0].content.len(), 1);
        assert_eq!(out[1].content.len(), 1);
        assert_eq!(out[2].content.len(), 2);
        let ContentBlock::Text { text: tail } = &out[2].content[1] else {
            panic!("expected text tail block");
        };
        assert!(tail.starts_with("<turn-context>"));
        assert!(tail.contains("fresh state"));

        assert_eq!(messages[2].content.len(), 1);
    }

    #[test]
    fn append_recency_tail_no_user_role_message_is_noop() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "assistant reply".into(),
            }],
            ..Default::default()
        }];
        let mut facts = crate::prompt::RecencyFacts::new();
        facts.push("x".into());
        assert!(append_recency_tail(&messages, &facts).is_none());
    }
}

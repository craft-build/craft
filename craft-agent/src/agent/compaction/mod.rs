use std::env;

use craft_providers::{Model, TokenUsage};

use super::history::History;

mod llm;
mod progressive;
mod strip;
pub(crate) mod vcc;
mod vcc_glue;
pub(crate) mod vcc_recall;

#[cfg(test)]
mod test_support;

pub use llm::compact;
pub(crate) use llm::{CONTINUE_AFTER_COMPACT, MAX_TOKEN_ESTIMATION_MULTIPLIER, compact_history};
pub(crate) use progressive::{CompactContext, progressive_compact};
pub(crate) use vcc_glue::vcc_compact;

const IMAGE_PLACEHOLDER: &str = "[image]";
const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";
const KEEP_LAST_TOOL_RESULTS: usize = 3;
const SUMMARY_MARKER_PREFIX: &str = "[Summary: ";
const PROGRESSIVE_TOOL_REMOVAL_RATIOS: &[f32] = &[0.10, 0.20, 0.50, 1.00];

const AGGRESSIVE_CODE_RATE: f32 = 0.15;
const AGGRESSIVE_MAX_LOG_LINES: usize = 20;
const AGGRESSIVE_MAX_DIFF_LINES: usize = 40;
const AGGRESSIVE_MAX_SEARCH_FILES: usize = 10;
const AGGRESSIVE_MAX_JSON_ITEMS: usize = 8;
const MIN_TOOL_RESULT_CHARS: usize = 300;
const LOW_RELEVANCE_THRESHOLD: f32 = 0.3;
const HIGH_RELEVANCE_THRESHOLD: f32 = 0.7;
const VERY_OLD_MULTIPLIER: usize = 3;
const SUMMARY_PREVIEW_CHARS: usize = 80;
const SUFFICIENT_REDUCTION_RATIO: f32 = 0.15;
const ERROR_SNIPPET_CHARS: usize = 200;

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, compaction_buffer: u32) -> bool {
    let usable = model.context_window.saturating_sub(compaction_buffer);
    usage.context_tokens() >= usable
}

pub(super) fn is_proactive_threshold(
    history: &History,
    model: &Model,
    ratio: f64,
    multiplier: f64,
) -> bool {
    let estimated = (history.estimate_tokens(model) as f64 * multiplier) as u32;
    let threshold = (model.context_window as f64 * ratio) as u32;
    estimated >= threshold
}

pub(super) fn context_under_limit(
    history: &History,
    model: &Model,
    compaction_buffer: u32,
    multiplier: f64,
) -> bool {
    let usable = model.context_window.saturating_sub(compaction_buffer);
    let estimated = (history.estimate_tokens(model) as f64 * multiplier) as u32;
    estimated < usable
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("CRAFT_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::test_support::{default_model, small_context_model};
    use super::*;
    use craft_providers::ContentBlock;
    use craft_providers::{Message, Role};

    #[test]
    fn proactive_threshold_detects_large_history() {
        let model = small_context_model(1000);
        let long_text: String = "x".repeat(4000);
        let history = History::new(vec![
            Message::user(long_text.clone()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: long_text }],
                ..Default::default()
            },
        ]);
        assert!(
            is_proactive_threshold(&history, &model, 0.50, 1.0),
            "should exceed 50% threshold"
        );
    }

    #[test]
    fn proactive_threshold_false_for_small_history() {
        let model = small_context_model(200_000);
        let history = History::new(vec![Message::user("hello".into())]);
        assert!(
            !is_proactive_threshold(&history, &model, 0.75, 1.0),
            "should not exceed 75% threshold"
        );
    }

    #[test]
    fn proactive_threshold_multiplier_fires_earlier() {
        let model = small_context_model(1000);
        let history = History::new(vec![Message::user("x".repeat(1000))]);
        assert!(
            !is_proactive_threshold(&history, &model, 0.50, 1.0),
            "should not exceed 50% threshold with multiplier=1.0"
        );
        assert!(
            is_proactive_threshold(&history, &model, 0.50, 2.0),
            "should exceed 50% threshold with multiplier=2.0"
        );
    }

    #[test]
    fn context_under_limit_multiplier_makes_stricter() {
        let model = small_context_model(1000);
        let history = History::new(vec![Message::user("x".repeat(2000))]);
        assert!(
            context_under_limit(&history, &model, 0, 1.0),
            "should be under limit with multiplier=1.0"
        );
        assert!(
            !context_under_limit(&history, &model, 0, 2.0),
            "should not be under limit with multiplier=2.0"
        );
    }

    #[test]
    fn default_model_resolves() {
        let m = default_model();
        assert!(m.context_window > 0);
    }
}

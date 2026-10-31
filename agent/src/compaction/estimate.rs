//! Rough token estimation for conversation history (ported from Craft's
//! compaction estimator). Deliberately approximate; the engine's
//! effectiveness score keeps behavior safe when estimates drift.

use animist::history::{ContentBlock, Message, StoredMessage, ToolContent};

const CHARS_PER_TOKEN: usize = 4;

/// Flat per-message structure cost (ChatML-style message framing), folded
/// into every [`estimate_tokens`] result. Conservative: most providers
/// charge 3-5 tokens of structure per message.
pub const TOKENS_PER_MESSAGE: u64 = 5;

/// Upper bound for the calibration multiplier: a runaway loop of overflow
/// recalibrations must not push estimates into "always compact" territory.
pub const MAX_TOKEN_ESTIMATION_MULTIPLIER: f64 = 5.0;

/// Calibrated token estimation: `chars / 4` scaled by a multiplier that grows
/// when the provider's actual usage proves the estimate too low (ported from
/// Craft's overflow recalibration: `actual / estimated × 1.1`, clamped).
#[derive(Clone, Copy, Debug)]
pub struct TokenEstimator {
    multiplier: f64,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self { multiplier: 1.0 }
    }
}

impl TokenEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn multiplier(&self) -> f64 {
        self.multiplier
    }

    /// Scale a raw estimate by the calibrated multiplier.
    pub fn scale(&self, raw: u64) -> u64 {
        (raw as f64 * self.multiplier) as u64
    }

    /// Recalibrate after an overflow proved the estimate low: raise the
    /// multiplier to `actual / estimated × 1.1` (never lowered, never past
    /// [`MAX_TOKEN_ESTIMATION_MULTIPLIER`]). Returns whether it moved.
    pub fn recalibrate(&mut self, actual: u64, estimated: u64) -> bool {
        if estimated == 0 || actual == 0 {
            return false;
        }
        let ratio = actual as f64 / estimated as f64;
        if ratio > self.multiplier {
            self.multiplier = (ratio * 1.1).min(MAX_TOKEN_ESTIMATION_MULTIPLIER);
            return true;
        }
        false
    }
}

fn text_len(text: &str) -> usize {
    // UTF-8 bytes: multibyte text (CJK, emoji) costs more tokens per
    // character than the ASCII-calibrated 4-per-unit rule assumes, so
    // byte length keeps the estimate conservative where characters
    // would undercount it.
    text.len()
}

/// Flat token weight for one image block (Anthropic's pricing for images
/// sent as vision input, per the reference's D.1 estimator).
pub const TOKENS_PER_IMAGE: u64 = 1500;

/// Estimate the prompt tokens for `messages`: chars aggregated across all
/// messages before the single `/4` rounding (N short messages must not
/// estimate as 0 in aggregate), plus a flat structure cost per message.
/// Image blocks are weighted at `TOKENS_PER_IMAGE`.
pub fn estimate_tokens(messages: &[StoredMessage]) -> u64 {
    let mut chars: usize = 0;
    for stored in messages {
        chars += stored
            .message()
            .content
            .iter()
            .map(block_chars)
            .sum::<usize>();
    }
    (chars / CHARS_PER_TOKEN) as u64 + messages.len() as u64 * TOKENS_PER_MESSAGE
}

/// The same estimate over wire messages (the request view).
pub fn estimate_messages(messages: &[Message]) -> u64 {
    let mut chars: usize = 0;
    for message in messages {
        chars += message.content.iter().map(block_chars).sum::<usize>();
    }
    (chars / CHARS_PER_TOKEN) as u64 + messages.len() as u64 * TOKENS_PER_MESSAGE
}

fn block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text_len(text),
        // Attached images weigh like tool-result images: a flat per-image
        // token cost, in chars so the `/4` division stays consistent.
        ContentBlock::Image { .. } => TOKENS_PER_IMAGE as usize * CHARS_PER_TOKEN,
        ContentBlock::Thinking { thinking, .. } => text_len(thinking),
        ContentBlock::RedactedThinking { data } => text_len(data),
        ContentBlock::ToolUse { name, input, .. } => text_len(name) + json_len(input) as usize,
        ContentBlock::ToolResult { content, .. } => content
            .0
            .iter()
            .map(|item| match item {
                ToolContent::Text { text } => text_len(text),
                // Flat weight expressed in chars so the `/4` division lands
                // on `TOKENS_PER_IMAGE`.
                ToolContent::Image { .. } => TOKENS_PER_IMAGE as usize * CHARS_PER_TOKEN,
            })
            .sum(),
    }
}

fn json_len(value: &serde_json::Value) -> u64 {
    value.to_string().len() as u64
}

/// Token cost of everything a request carries besides its messages: the
/// system preamble and the serialized tool schemas, which servers enforcing
/// `prompt + max_tokens <= context_window` count against the same budget.
/// Built once per run/turn (never per event or loop iteration) and cached on
/// the shared [`crate::compaction::CompactionState`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestOverhead {
    tokens: u64,
}

impl RequestOverhead {
    /// Overhead from a preamble and the tool declarations the request will
    /// actually carry. Per tool: name + description + serialized input
    /// schema; all chars aggregated and rounded once.
    pub fn from_parts(preamble: Option<&str>, tools: &[animist::provider::Tool]) -> Self {
        let mut chars = preamble.map(str::len).unwrap_or(0);
        for tool in tools {
            chars += tool.name.len();
            chars += tool.description.len();
            chars += tool.input_schema.to_string().len();
        }
        Self::from_chars(chars)
    }

    fn from_chars(chars: usize) -> Self {
        Self {
            tokens: (chars / CHARS_PER_TOKEN) as u64,
        }
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
}

/// Prompt estimate including what the message list leaves out: the system
/// preamble and the serialized tool schemas (see [`RequestOverhead`]).
pub fn estimate_prompt_tokens(
    messages: &[Message],
    system: &str,
    tools: &serde_json::Value,
) -> u64 {
    let overhead = RequestOverhead::from_chars(system.len() + json_len(tools) as usize);
    estimate_messages(messages).saturating_add(overhead.tokens())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ext;

    fn user(text: &str) -> Message {
        ext::user_text(text)
    }

    /// The stored-history view of wire messages, for `estimate_tokens`.
    fn stored(messages: Vec<Message>) -> Vec<StoredMessage> {
        messages.into_iter().map(ext::stored).collect()
    }

    #[test]
    fn estimates_by_chars_per_token() {
        // 400 chars / 4 = 100 tokens, plus one message's structure cost.
        let messages = stored(vec![user(&"x".repeat(400))]);
        assert_eq!(estimate_tokens(&messages), 100 + TOKENS_PER_MESSAGE);
    }

    #[test]
    fn zero_for_empty_history() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn short_messages_aggregate_before_rounding() {
        // 100 one-char messages: 0 tokens each under per-message rounding,
        // 25 from the aggregated chars plus 500 structure tokens together.
        let messages = stored((0..100).map(|_| user("a")).collect());
        assert_eq!(estimate_tokens(&messages), 25 + 100 * TOKENS_PER_MESSAGE);
    }

    #[test]
    fn single_short_message_counts_its_structure_cost() {
        // A lone short message must not estimate as zero either.
        assert_eq!(
            estimate_tokens(&stored(vec![user("a")])),
            TOKENS_PER_MESSAGE
        );
    }

    #[test]
    fn per_message_structure_constant_is_conservative() {
        assert_eq!(TOKENS_PER_MESSAGE, 5);
    }

    #[test]
    fn images_weigh_a_flat_token_cost() {
        // Image blocks count as their flat weight, not their (huge) base64
        // payload; the flat char weight keeps the `/4` at TOKENS_PER_IMAGE.
        let image = ContentBlock::Image {
            source: ext::image_source(ext::ImageMedia::Png, "z".repeat(4096)),
        };
        let messages = stored(vec![Message {
            role: animist::history::Role::User,
            content: vec![image],
            display_text: None,
            kind: animist::history::MessageKind::Turn,
        }]);
        assert_eq!(
            estimate_tokens(&messages),
            TOKENS_PER_IMAGE + TOKENS_PER_MESSAGE
        );
    }

    #[test]
    fn request_overhead_sums_preamble_and_tool_schemas() {
        let tools = vec![
            animist::provider::Tool::builder()
                .name("bash")
                .description("run a command")
                .input_schema(serde_json::json!({"type": "object"}))
                .build(),
            animist::provider::Tool::builder()
                .name("read")
                .description("read")
                .input_schema(serde_json::Value::Null)
                .build(),
        ];
        let chars = "preamble text".len()
            + tools
                .iter()
                .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
                .sum::<usize>();
        let overhead = RequestOverhead::from_parts(Some("preamble text"), &tools);
        assert_eq!(overhead.tokens(), (chars / 4) as u64);
        assert!(overhead.tokens() > 0);
        // Each tool's schema contributes.
        let one = RequestOverhead::from_parts(Some("preamble text"), &tools[..1]);
        assert!(one.tokens() < overhead.tokens());
    }

    #[test]
    fn request_overhead_empty_is_zero() {
        assert_eq!(RequestOverhead::from_parts(None, &[]).tokens(), 0);
        assert_eq!(RequestOverhead::default().tokens(), 0);
        // An empty preamble counts as absent.
        assert_eq!(RequestOverhead::from_parts(Some(""), &[]).tokens(), 0);
    }

    #[test]
    fn prompt_estimate_includes_system_and_tool_schemas() {
        let messages = vec![user(&"x".repeat(400))];
        let tools = serde_json::json!([{"name": "read", "parameters": {"type": "object"}}]);
        let overhead = (16 + tools.to_string().len()) / 4;
        assert_eq!(
            estimate_prompt_tokens(&messages, "system preamble!!", &tools),
            100 + TOKENS_PER_MESSAGE + overhead as u64
        );
        // The message-only estimate must not double-count the overhead.
        assert_eq!(estimate_tokens(&stored(messages)), 100 + TOKENS_PER_MESSAGE);
    }

    #[test]
    fn estimator_scales_by_multiplier() {
        let mut estimator = TokenEstimator::new();
        assert_eq!(estimator.scale(100), 100);
        assert!(estimator.recalibrate(300, 100));
        assert!((estimator.multiplier() - 3.3).abs() < 1e-9);
        assert_eq!(estimator.scale(100), 330);
    }

    #[test]
    fn recalibration_only_raises_and_never_past_cap() {
        let mut estimator = TokenEstimator::new();
        // Actual below the estimate never lowers the multiplier.
        assert!(!estimator.recalibrate(50, 100));
        assert_eq!(estimator.multiplier(), 1.0);
        // Ratio below the current multiplier does not move it either.
        assert!(estimator.recalibrate(200, 100));
        assert!(!estimator.recalibrate(150, 100));
        // Ratio far beyond the cap clamps at 5.0.
        assert!(estimator.recalibrate(100_000, 100));
        assert_eq!(estimator.multiplier(), MAX_TOKEN_ESTIMATION_MULTIPLIER);
    }

    #[test]
    fn recalibration_ignores_degenerate_inputs() {
        let mut estimator = TokenEstimator::new();
        assert!(!estimator.recalibrate(0, 100));
        assert!(!estimator.recalibrate(100, 0));
        assert_eq!(estimator.multiplier(), 1.0);
    }
}

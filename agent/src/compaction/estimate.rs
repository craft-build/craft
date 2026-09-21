//! Rough token estimation for conversation history (ported from Craft's
//! compaction estimator). Deliberately approximate; the engine's
//! effectiveness score keeps behavior safe when estimates drift.

use crate::history::{AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent};

const CHARS_PER_TOKEN: usize = 4;

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

/// Estimate the prompt tokens for `messages`, image blocks weighted at
/// `TOKENS_PER_IMAGE`.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let mut tokens: u64 = 0;
    for message in messages {
        match message {
            Message::System { content } => tokens += (text_len(content) / CHARS_PER_TOKEN) as u64,
            Message::User { content } => {
                tokens +=
                    (content.iter().map(user_content_chars).sum::<usize>() / CHARS_PER_TOKEN) as u64
            }
            Message::Assistant { content } => {
                tokens += (content.iter().map(assistant_content_chars).sum::<usize>()
                    / CHARS_PER_TOKEN) as u64
            }
        }
    }
    tokens
}

fn user_content_chars(block: &UserContent) -> usize {
    match block {
        UserContent::Text(text) => text_len(&text.text),
        UserContent::ToolResult(result) => result
            .content
            .iter()
            .map(|item| match item {
                ToolResultContent::Text(text) => text_len(&text.text),
                ToolResultContent::Json { value } => json_len(value) as usize,
                // Flat weight expressed in chars so the `/4` division lands
                // on `TOKENS_PER_IMAGE`.
                ToolResultContent::Image(_) => TOKENS_PER_IMAGE as usize * CHARS_PER_TOKEN,
            })
            .sum(),
    }
}

fn assistant_content_chars(block: &AssistantContent) -> usize {
    match block {
        AssistantContent::Text(text) => text_len(&text.text),
        AssistantContent::ToolCall(call) => {
            text_len(&call.function.name) + json_len(&call.function.arguments) as usize
        }
        AssistantContent::Reasoning(reasoning) => reasoning
            .content
            .iter()
            .map(|item| match item {
                ReasoningContent::Text { text } => text_len(text),
                ReasoningContent::Opaque(data) => text_len(data),
            })
            .sum(),
    }
}

fn json_len(value: &serde_json::Value) -> u64 {
    value.to_string().len() as u64
}

/// Prompt estimate including what the message list leaves out: the system
/// preamble and the serialized tool schemas, which servers enforcing
/// `prompt + max_tokens <= context_window` count against the same budget.
pub fn estimate_prompt_tokens(
    messages: &[Message],
    system: &str,
    tools: &serde_json::Value,
) -> u64 {
    let overhead = (system.len() as u64 + json_len(tools)) / CHARS_PER_TOKEN as u64;
    estimate_tokens(messages).saturating_add(overhead)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::text(text)],
        }
    }

    #[test]
    fn estimates_by_chars_per_token() {
        // 400 chars / 4 = 100 tokens.
        let messages = vec![user(&"x".repeat(400))];
        assert_eq!(estimate_tokens(&messages), 100);
    }

    #[test]
    fn zero_for_empty_history() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn prompt_estimate_includes_system_and_tool_schemas() {
        let messages = vec![user(&"x".repeat(400))];
        let tools = serde_json::json!([{"name": "read", "parameters": {"type": "object"}}]);
        let overhead = (16 + tools.to_string().len()) / 4;
        assert_eq!(
            estimate_prompt_tokens(&messages, "system preamble!!", &tools),
            100 + overhead as u64
        );
        // The message-only estimate must not double-count the overhead.
        assert_eq!(estimate_tokens(&messages), 100);
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

//! A UTF-8-safe reveal cursor; the conversation retains the complete text.

use std::time::{Duration, Instant};

const MS_PER_CHAR: u64 = 4;
const MIN_REVEAL_MS: u64 = 30;
const MAX_REVEAL_MS: u64 = 1000;

#[derive(Debug)]
pub(crate) struct Typewriter {
    visible_chars: usize,
    visible_bytes: usize,
    start_chars: usize,
    target_chars: usize,
    started: Instant,
    duration: Duration,
}

impl Typewriter {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            visible_chars: 0,
            visible_bytes: 0,
            start_chars: 0,
            target_chars: 0,
            started: now,
            duration: Duration::ZERO,
        }
    }

    /// `text` is the complete, append-only buffer, `delta` its new suffix.
    pub(crate) fn push(&mut self, text: &str, delta: &str, now: Instant) {
        if delta.is_empty() {
            return;
        }
        self.tick(text, now);
        self.target_chars += delta.chars().count();
        self.start_chars = self.visible_chars;
        self.started = now;
        let remaining = self.target_chars - self.start_chars;
        self.duration = Duration::from_millis(
            (remaining as u64 * MS_PER_CHAR).clamp(MIN_REVEAL_MS, MAX_REVEAL_MS),
        );
    }

    pub(crate) fn tick(&mut self, text: &str, now: Instant) -> bool {
        if !self.is_animating() {
            return false;
        }
        let progress = (now.saturating_duration_since(self.started).as_secs_f64()
            / self.duration.as_secs_f64())
        .min(1.0);
        let chars = self.start_chars
            + ((self.target_chars - self.start_chars) as f64 * progress).round() as usize;
        let skip = chars - self.visible_chars;
        if skip == 0 {
            return false;
        }
        self.visible_bytes += text[self.visible_bytes..]
            .char_indices()
            .nth(skip)
            .map_or(text.len() - self.visible_bytes, |(offset, _)| offset);
        self.visible_chars = chars;
        true
    }

    pub(crate) fn visible<'a>(&self, text: &'a str) -> &'a str {
        &text[..self.visible_bytes]
    }

    pub(crate) fn is_animating(&self) -> bool {
        self.visible_chars < self.target_chars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_and_delta_boundaries_preserve_progress() {
        let now = Instant::now();
        let mut tw = Typewriter::new(now);
        tw.push("é中🦀a", "é中🦀a", now);
        assert_eq!(tw.visible("é中🦀a"), "");
        tw.tick("é中🦀a", now + Duration::from_millis(15));
        assert_eq!(tw.visible("é中🦀a"), "é中");
        tw.push("é中🦀ab", "b", now + Duration::from_millis(15));
        assert_eq!(tw.visible("é中🦀ab"), "é中");
        tw.push("é中🦀ab", "", now + Duration::from_millis(20));
        tw.tick("é中🦀ab", now + Duration::from_millis(45));
        assert_eq!(tw.visible("é中🦀ab"), "é中🦀ab");
        assert!(!tw.is_animating());
    }

    #[test]
    fn duration_uses_four_ms_per_char_and_clamps() {
        for (chars, ms) in [(1, 30), (100, 400), (1000, 1000)] {
            let now = Instant::now();
            let text = "x".repeat(chars);
            let mut tw = Typewriter::new(now);
            tw.push(&text, &text, now);
            assert_eq!(tw.duration, Duration::from_millis(ms));
            tw.tick(&text, now + Duration::from_millis(ms));
            assert_eq!(tw.visible(&text), text);
            assert!(!tw.is_animating());
        }
    }
}

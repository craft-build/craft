/// Tracks which messages are in the provider's prefix KV cache.
///
/// When a provider (e.g. Anthropic) has cache breakpoints, messages up to
/// the last breakpoint are cached. Compressing those messages wastes the
/// cache read discount (Anthropic: 90% cheaper reads, 25% write penalty).
///
/// The tracker uses the `cache_read` token count from API responses to
/// estimate how many messages are frozen in the cache.
use craft_providers::TokenUsage;
use tracing::info;

/// Economics for Anthropic-style prefix caching.
const CACHE_READ_DISCOUNT: f32 = 0.90;
const CACHE_WRITE_PENALTY: f32 = 0.25;

pub(super) struct PrefixCacheTracker {
    /// Number of messages confirmed to be in the provider's KV cache.
    frozen_count: usize,
}

impl PrefixCacheTracker {
    pub(super) fn new() -> Self {
        Self { frozen_count: 0 }
    }

    /// Update the tracker after an API response. If `cache_read > 0`, we
    /// know the prefix up to the previous request was cached, so all messages
    /// that existed before this turn are in the cache.
    pub(super) fn update(&mut self, usage: &TokenUsage, history_len: usize) {
        if usage.cache_read > 0 {
            let new_frozen = history_len.saturating_sub(2);
            if new_frozen > self.frozen_count {
                info!(
                    old_frozen = self.frozen_count,
                    new_frozen,
                    cache_read_tokens = usage.cache_read,
                    "prefix cache tracker updated"
                );
                self.frozen_count = new_frozen;
            }
        }
    }

    /// Whether a message at the given index is in the frozen prefix cache.
    pub(super) fn is_frozen(&self, msg_index: usize) -> bool {
        msg_index < self.frozen_count
    }

    /// Cost-benefit check: should we compress a cached message?
    /// Returns true if the net token savings outweigh the lost cache read discount.
    /// The write penalty on the compressed version is small (25% of compressed size),
    /// while we lose the read discount (90%) on the original size we no longer send.
    pub(super) fn should_compress(
        &self,
        msg_index: usize,
        original_chars: usize,
        compressed_chars: usize,
    ) -> bool {
        if !self.is_frozen(msg_index) {
            return true;
        }
        // We lose the read discount on the original: cost = original * 0.10 (base cost)
        // We pay base cost on compressed: cost = compressed * 1.0
        // We pay write penalty on compressed: cost = compressed * 0.25
        // Savings = (original * 0.10) - (compressed * 1.25)
        let original_cost = original_chars as f32 * (1.0 - CACHE_READ_DISCOUNT);
        let new_cost = compressed_chars as f32 * (1.0 + CACHE_WRITE_PENALTY);
        new_cost < original_cost
    }

    #[cfg(any(test, feature = "onnx"))]
    pub(super) fn frozen_count(&self) -> usize {
        self.frozen_count
    }
}

#[cfg(test)]
fn filter_frozen<'a>(tracker: &PrefixCacheTracker, candidates: &'a [usize]) -> Vec<&'a usize> {
    candidates
        .iter()
        .filter(|&&idx| !tracker.is_frozen(idx))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_has_zero_frozen() {
        let tracker = PrefixCacheTracker::new();
        assert_eq!(tracker.frozen_count(), 0);
        assert!(!tracker.is_frozen(0));
    }

    #[test]
    fn update_sets_frozen_on_cache_read() {
        let mut tracker = PrefixCacheTracker::new();
        let usage = TokenUsage {
            input: 100,
            cache_read: 50_000,
            cache_creation: 0,
            output: 50,
        };
        tracker.update(&usage, 10);
        assert!(tracker.is_frozen(5));
        assert!(tracker.is_frozen(7));
        assert!(!tracker.is_frozen(8));
    }

    #[test]
    fn update_ignores_zero_cache_read() {
        let mut tracker = PrefixCacheTracker::new();
        let usage = TokenUsage {
            input: 100,
            cache_read: 0,
            cache_creation: 0,
            output: 50,
        };
        tracker.update(&usage, 10);
        assert_eq!(tracker.frozen_count(), 0);
    }

    #[test]
    fn update_only_grows_never_shrinks() {
        let mut tracker = PrefixCacheTracker::new();
        let usage = TokenUsage {
            input: 100,
            cache_read: 50_000,
            cache_creation: 0,
            output: 50,
        };
        tracker.update(&usage, 10);
        assert_eq!(tracker.frozen_count(), 8);
        tracker.update(&usage, 6);
        assert_eq!(tracker.frozen_count(), 8);
    }

    #[test]
    fn should_compress_skips_frozen_unless_worth_it() {
        let mut tracker = PrefixCacheTracker::new();
        let usage = TokenUsage {
            input: 100,
            cache_read: 50_000,
            cache_creation: 0,
            output: 50,
        };
        tracker.update(&usage, 10);

        assert!(!tracker.should_compress(0, 1000, 500));
        assert!(tracker.should_compress(0, 1000, 50));
        assert!(tracker.should_compress(9, 1000, 999));
    }

    #[test]
    fn filter_frozen_removes_cached_indices() {
        let mut tracker = PrefixCacheTracker::new();
        let usage = TokenUsage {
            input: 100,
            cache_read: 50_000,
            cache_creation: 0,
            output: 50,
        };
        tracker.update(&usage, 10);

        let candidates = vec![0, 2, 5, 8, 9];
        let filtered = filter_frozen(&tracker, &candidates);
        assert_eq!(filtered.len(), 2);
        assert_eq!(*filtered[0], 8);
        assert_eq!(*filtered[1], 9);
    }
}

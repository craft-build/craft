//! Usage & cost accounting.
//!
//! A turn is priced once, when it runs, and that number is what gets stored,
//! summed and shown; history is never re-priced, because rates move. Rates
//! come from the serving model's catalog entry (`ModelInfo.pricing`):
//! a model the catalog leaves unreported is unpriced — callers get `None`
//! and show no cost instead of a made-up "$0.000".

use std::collections::HashMap;
use std::ops::AddAssign;

use animist::provider::Pricing;
use serde::{Deserialize, Serialize};

const PER_MILLION: f64 = 1_000_000.0;

/// Cache multipliers Anthropic apply on top of the base input rate; animist
/// keeps the same consts private pending F08.
pub(crate) const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
pub(crate) const CACHE_READ_MULTIPLIER: f64 = 0.10;

/// Token usage reported by the provider for one model call: animist's
/// counters, re-exported so every surface reads one type. The rate math
/// stays here because animist 0.7.0 leaves `cost` unset (followup F08 is
/// open).
pub use animist::provider::TokenUsage;

/// Whether every counter is zero.
pub fn is_zero(usage: &TokenUsage) -> bool {
    usage.input == 0 && usage.output == 0 && usage.cache_creation == 0 && usage.cache_read == 0
}

/// Fold a later report into a running total.
pub fn accumulate(total: &mut TokenUsage, other: &TokenUsage) {
    total.input = total.input.saturating_add(other.input);
    total.output = total.output.saturating_add(other.output);
    total.cache_creation = total.cache_creation.saturating_add(other.cache_creation);
    total.cache_read = total.cache_read.saturating_add(other.cache_read);
}

/// Ready to store, with what the turn was billed. Not a `From<TokenUsage>`
/// on purpose: a caller that forgets the cost quietly loses money from
/// the session total.
pub fn billed(usage: &TokenUsage, cost: Option<f64>) -> StoredTokenUsage {
    StoredTokenUsage {
        input: usage.input,
        output: usage.output,
        cache_creation: usage.cache_creation,
        cache_read: usage.cache_read,
        cost,
    }
}

pub fn total_input(usage: &TokenUsage) -> u32 {
    usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_creation)
}

pub fn context_tokens(usage: &TokenUsage) -> u32 {
    total_input(usage).saturating_add(usage.output)
}

pub fn format(usage: &TokenUsage, cost: Option<f64>) -> String {
    format_cost(usage, cost, "")
}

/// Like [`format`], but marks the cost as a running total.
pub fn format_sum_cost(usage: &TokenUsage, cost: Option<f64>) -> String {
    format_cost(usage, cost, "Σ")
}

fn format_cost(usage: &TokenUsage, cost: Option<f64>, prefix: &str) -> String {
    let tokens = format!(
        "{}\u{2191} {}\u{2193}",
        format_tokens(total_input(usage)),
        format_tokens(usage.output)
    );
    match cost {
        Some(cost) => format!("{tokens} {prefix}${cost:.3}"),
        None => tokens,
    }
}

/// One model call's cost from catalog rates: `None` when the catalog does
/// not report the input and output rates, and when every reported rate is
/// zero — unpriced, not free. Missing cache rates contribute nothing. Fast
/// mode swaps in the fast tier's rates when one exists, with its cache
/// rates derived from its own input rate.
pub(crate) fn cost(usage: &TokenUsage, pricing: &Pricing, fast: bool) -> Option<f64> {
    let (input, output, cache_write, cache_read) = match &pricing.fast {
        Some(f) if fast => (
            f.input,
            f.output,
            Some(f.input * CACHE_WRITE_MULTIPLIER),
            Some(f.input * CACHE_READ_MULTIPLIER),
        ),
        _ => (
            pricing.input?,
            pricing.output?,
            pricing.cache_write,
            pricing.cache_read,
        ),
    };
    let (cache_write, cache_read) = (cache_write.unwrap_or(0.0), cache_read.unwrap_or(0.0));
    let known_zero = input == 0.0 && output == 0.0 && cache_write == 0.0 && cache_read == 0.0;
    (!known_zero).then(|| {
        usage.input as f64 * input / PER_MILLION
            + usage.output as f64 * output / PER_MILLION
            + usage.cache_creation as f64 * cache_write / PER_MILLION
            + usage.cache_read as f64 * cache_read / PER_MILLION
    })
}

/// A [`TokenUsage`] plus what it was billed, ready for the session ledger.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_creation: u32,
    #[serde(default)]
    pub cache_read: u32,
    /// What the turns that made up this entry actually paid, when they
    /// recorded it. `None` means the entry holds legacy counters only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

impl From<StoredTokenUsage> for TokenUsage {
    fn from(s: StoredTokenUsage) -> Self {
        Self {
            input: s.input,
            output: s.output,
            cache_creation: s.cache_creation,
            cache_read: s.cache_read,
            cost: None,
        }
    }
}

impl StoredTokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn total(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }
}

impl AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        let cost = add_cost_opt(self.cost, rhs.cost);
        let mut tokens = TokenUsage::from(*self);
        accumulate(&mut tokens, &TokenUsage::from(rhs));
        *self = billed(&tokens, cost);
    }
}

/// The one way costs are summed, so every running total agrees: `None` until
/// the first priced turn shows up, and from there it only grows.
pub fn add_cost_opt(total: Option<f64>, addend: Option<f64>) -> Option<f64> {
    match (total, addend) {
        (Some(a), Some(b)) => Some(a + b),
        (cost, None) | (None, cost) => cost,
    }
}

pub fn format_tokens(tokens: impl Into<u64>) -> String {
    let tokens = tokens.into();
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}m", tokens as f64 / 1_000_000.0),
    }
}

/// The bill a session ran up; status bar, `/usage`, ACP and headless all
/// come here so they cannot disagree.
///
/// Turns record what they paid, so summing those is the truth. Counters
/// written before that kept no cost; the serving model's rates estimate
/// them once — after a later turn merges its own cost in, the counters no
/// longer say which of them was already paid for, so estimating again would
/// drop everything the entry had before. `None` when nothing here is
/// priced.
pub fn settle_session(
    total: &TokenUsage,
    by_model: &mut HashMap<String, StoredTokenUsage>,
    current_spec: &str,
    pricing: &Pricing,
    fast: bool,
) -> Option<f64> {
    if by_model.is_empty() && !is_zero(total) {
        by_model.insert(current_spec.to_owned(), billed(total, None));
    }
    for (spec, usage) in by_model.iter_mut() {
        usage.cost = model_cost(spec, usage, current_spec, pricing, fast);
    }
    by_model
        .values()
        .filter_map(|usage| usage.cost)
        .reduce(|total, cost| total + cost)
}

/// One model's slice of [`settle_session`], as `/usage` breaks it down per
/// row. A recorded cost always wins; otherwise only the serving model's row
/// is priced, from the rates the caller supplies for it. Everything else —
/// a spec another provider served, or another model of the same provider —
/// keeps `None`: those counters price from what their own turns recorded.
pub fn model_cost(
    spec: &str,
    usage: &StoredTokenUsage,
    current_spec: &str,
    pricing: &Pricing,
    fast: bool,
) -> Option<f64> {
    if let Some(cost) = usage.cost {
        return Some(cost);
    }
    identifies_serving_model(spec, current_spec)?;
    cost(&(*usage).into(), pricing, fast)
}

/// Whether `spec` names the model `current_spec` does: the provider half
/// (implicit in a bare legacy id) must match, and the id must be the
/// serving id or a dated snapshot of it (`claude-x-20260101` extends
/// `claude-x`).
fn identifies_serving_model(spec: &str, current_spec: &str) -> Option<()> {
    let (current_provider, current_id) = current_spec.split_once('/')?;
    let (spec_provider, spec_id) = match spec.split_once('/') {
        Some((provider, id)) => (provider, id),
        None => (current_provider, spec),
    };
    (spec_provider == current_provider && spec_id.starts_with(current_id)).then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const MILLION: u32 = 1_000_000;
    /// Four counters that cannot be confused with each other.
    const COUNTERS: TokenUsage = TokenUsage {
        input: 11,
        output: 22,
        cache_creation: 33,
        cache_read: 44,
        cost: None,
    };

    fn pricing(input: f64, output: f64) -> Pricing {
        Pricing {
            input: Some(input),
            output: Some(output),
            cache_write: None,
            cache_read: None,
            fast: None,
        }
    }

    #[test_case(999, "999"         ; "under_thousand")]
    #[test_case(1_000, "1.0k"      ; "thousand")]
    #[test_case(999_999, "1000.0k" ; "just_under_million")]
    #[test_case(1_000_000, "1.0m"  ; "million")]
    fn format_tokens_display(tokens: u32, expected: &str) {
        assert_eq!(format_tokens(tokens), expected);
    }

    #[test_case(TokenUsage { input: 12_000, output: 456, cache_creation: 200, cache_read: 100, cost: None }, None, "12.3k\u{2191} 456\u{2193}" ; "without_cost")]
    #[test_case(TokenUsage { input: 1_000_000, output: 100_000, cache_creation: 200_000, cache_read: 500_000, cost: None }, Some(5.4), "1.7m\u{2191} 100.0k\u{2193} $5.400" ; "with_cost")]
    #[test_case(TokenUsage { input: u32::MAX, output: 1, cache_creation: 1, cache_read: 1, cost: None }, None, "4295.0m\u{2191} 1\u{2193}" ; "input_saturates")]
    fn usage_formatting(usage: TokenUsage, cost: Option<f64>, expected: &str) {
        assert_eq!(format(&usage, cost), expected);
    }

    #[test]
    fn each_counter_is_billed_at_its_own_rate() {
        let usage = TokenUsage {
            input: MILLION,
            output: MILLION,
            cache_creation: MILLION,
            cache_read: MILLION,
            cost: None,
        };
        let rates = Pricing {
            input: Some(1.0),
            output: Some(10.0),
            cache_write: Some(1.25),
            cache_read: Some(0.1),
            fast: None,
        };
        assert_eq!(cost(&usage, &rates, false), Some(1.0 + 10.0 + 1.25 + 0.1));
    }

    #[test_case(None, Some(10.0) ; "missing_input")]
    #[test_case(Some(1.0), None  ; "missing_output")]
    #[test_case(None, None       ; "missing_both")]
    fn unreported_rates_price_to_none(input: Option<f64>, output: Option<f64>) {
        let usage = TokenUsage {
            input: MILLION,
            output: MILLION,
            cost: None,
            ..Default::default()
        };
        let rates = Pricing {
            input,
            output,
            cache_write: Some(1.0),
            cache_read: Some(0.1),
            fast: None,
        };
        assert_eq!(cost(&usage, &rates, false), None);
    }

    /// A catalog that reports every rate as zero is not a confident "$0.000".
    #[test]
    fn all_zero_rates_price_to_none() {
        let usage = TokenUsage {
            input: MILLION,
            cost: None,
            ..Default::default()
        };
        let rates = Pricing {
            input: Some(0.0),
            output: Some(0.0),
            cache_write: Some(0.0),
            cache_read: Some(0.0),
            fast: None,
        };
        assert_eq!(cost(&usage, &rates, false), None);
        assert_eq!(cost(&usage, &rates, true), None);
    }

    /// Cache rates the catalog omits contribute zero, not an error.
    #[test]
    fn missing_cache_rates_contribute_nothing() {
        let usage = TokenUsage {
            input: MILLION,
            cache_creation: MILLION,
            cache_read: MILLION,
            cost: None,
            ..Default::default()
        };
        let rates = pricing(2.0, 10.0);
        assert_eq!(cost(&usage, &rates, false), Some(2.0));
    }

    #[test_case(false ; "standard_rates")]
    #[test_case(true ; "fast_rates")]
    fn fast_mode_replaces_the_reported_rates(fast: bool) {
        let rates = Pricing {
            input: Some(1.0),
            output: Some(2.0),
            cache_write: Some(0.0),
            cache_read: Some(0.0),
            fast: Some(animist::provider::FastPricing {
                input: 4.0,
                output: 8.0,
            }),
        };
        let usage = TokenUsage {
            input: MILLION,
            output: MILLION,
            cache_creation: MILLION,
            cache_read: MILLION,
            cost: None,
        };
        // Cache rates under fast mode derive from the fast input rate.
        let expected = if fast {
            Some(4.0 + 8.0 + 4.0 * CACHE_WRITE_MULTIPLIER + 4.0 * CACHE_READ_MULTIPLIER)
        } else {
            Some(1.0 + 2.0)
        };
        assert_eq!(cost(&usage, &rates, fast), expected);
    }

    /// Asking a model with no fast tier for fast mode quietly falls back to
    /// the standard rates instead of overcharging.
    #[test]
    fn fast_preference_without_a_fast_tier_uses_standard_rates() {
        let usage = TokenUsage {
            input: MILLION,
            cost: None,
            ..Default::default()
        };
        let rates = pricing(3.0, 15.0);
        assert_eq!(cost(&usage, &rates, true), cost(&usage, &rates, false));
    }

    #[test]
    fn billed_keeps_counters_and_add_saturates() {
        let stored = billed(&COUNTERS, Some(0.25));
        assert_eq!(
            stored,
            StoredTokenUsage {
                input: 11,
                output: 22,
                cache_creation: 33,
                cache_read: 44,
                cost: Some(0.25),
            }
        );
        let mut sum = TokenUsage {
            input: u32::MAX,
            ..Default::default()
        };
        accumulate(&mut sum, &COUNTERS);
        assert_eq!(sum.input, u32::MAX);
        assert_eq!(sum.output, COUNTERS.output);
    }

    const CURRENT: &str = "anthropic/claude-sonnet-5";
    const RATES: Pricing = Pricing {
        input: Some(2.0),
        output: Some(10.0),
        cache_write: Some(2.5),
        cache_read: Some(0.2),
        fast: None,
    };
    /// 1M input tokens at the serving model's input rate (2.00).
    const LIST_PRICE: f64 = 2.0;
    const RECORDED: f64 = 0.5;
    const UNRESOLVABLE: &str = "test/a-model-no-catalog-has-ever-heard-of";
    const ALSO_UNRESOLVABLE: &str = "test/another-model-no-catalog-has-ever-heard-of";

    fn stored(cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: MILLION,
            cost,
            ..Default::default()
        }
    }

    fn breakdown(entries: &[(&str, Option<f64>)]) -> HashMap<String, StoredTokenUsage> {
        entries
            .iter()
            .map(|(id, cost)| ((*id).to_owned(), stored(*cost)))
            .collect()
    }

    /// Each entry bills 1M input tokens, so a row worth [`LIST_PRICE`] came
    /// from the serving rates and [`RECORDED`] came from the turn itself. A
    /// breakdown nothing can price is not a free session: `Some(0.0)` would
    /// put a confident "$0.000" on screen.
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (ALSO_UNRESOLVABLE, Some(RECORDED))], Some(2.0 * RECORDED) ; "recorded_costs_win_over_the_serving_rates")]
    #[test_case(&[(CURRENT, None), (UNRESOLVABLE, None)], Some(LIST_PRICE)                                   ; "legacy_counters_use_the_serving_rates")]
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (CURRENT, None), (ALSO_UNRESOLVABLE, None)], Some(RECORDED + LIST_PRICE) ; "mixed_eras_sum_recorded_and_listed")]
    #[test_case(&[(UNRESOLVABLE, None)], None                                                                ; "a_breakdown_that_prices_to_nothing")]
    #[test_case(&[], Some(LIST_PRICE)                                                                        ; "no_breakdown_prices_the_total")]
    fn session_cost_bills_every_breakdown(entries: &[(&str, Option<f64>)], expected: Option<f64>) {
        let mut by_model = breakdown(entries);
        let counted = entries.len().max(1) as u32 * MILLION;

        // With a breakdown the stored running total says nothing about the
        // bill, so the two drifting apart (compaction, a resume) must not
        // change the answer.
        let totals = if entries.is_empty() {
            vec![counted]
        } else {
            vec![counted, 0, 999 * MILLION]
        };
        for input in totals {
            let total = TokenUsage {
                input,
                ..Default::default()
            };
            assert_eq!(
                settle_session(&total, &mut by_model, CURRENT, &RATES, false),
                expected,
                "total of {input} input tokens"
            );
        }
    }

    /// A stored cost breakdown survives the later turns that touch the
    /// entry; loads never re-report a stale single-turn cost.
    #[test_case(&[]                ; "counters_with_no_breakdown")]
    #[test_case(&[(CURRENT, None)] ; "a_breakdown_written_before_costs")]
    fn a_settled_estimate_survives_the_next_turn(entries: &[(&str, Option<f64>)]) {
        let mut by_model = breakdown(entries);
        let total = TokenUsage {
            input: MILLION,
            ..Default::default()
        };

        assert_eq!(
            settle_session(&total, &mut by_model, CURRENT, &RATES, false),
            Some(LIST_PRICE)
        );

        *by_model.entry(CURRENT.to_owned()).or_default() += stored(Some(RECORDED));

        assert_eq!(
            settle_session(&total, &mut by_model, CURRENT, &RATES, false),
            Some(LIST_PRICE + RECORDED)
        );
    }

    #[test]
    fn no_breakdown_on_an_unpriced_model_prices_to_nothing() {
        let unpriced = "ollama/whatever";
        let mut by_model = HashMap::new();
        let total = TokenUsage {
            input: MILLION,
            ..Default::default()
        };
        let rates = Pricing::default();
        assert_eq!(
            settle_session(&total, &mut by_model, unpriced, &rates, false),
            None
        );
        // Seeded with no price, like any other unresolvable entry.
        assert_eq!(by_model[unpriced].cost, None);
    }

    #[test_case("anthropic/claude-sonnet-5"             ; "the_serving_spec")]
    #[test_case("anthropic/claude-sonnet-5-20260101"    ; "a_dated_snapshot_of_the_serving_id")]
    #[test_case("claude-sonnet-5"                       ; "a_bare_legacy_id")]
    #[test_case("claude-sonnet-5-20260101"              ; "a_bare_dated_snapshot")]
    #[test_case("anthropic/claude-opus-5"               ; "another_model_of_the_serving_provider")]
    #[test_case("openai/claude-sonnet-5"                ; "the_serving_id_under_another_provider")]
    #[test_case("anthropic/gpt-6-astra"                 ; "another_model_entirely")]
    fn only_the_serving_model_prices_from_the_callers_rates(spec: &str) {
        let serving = matches!(
            spec,
            "anthropic/claude-sonnet-5"
                | "anthropic/claude-sonnet-5-20260101"
                | "claude-sonnet-5"
                | "claude-sonnet-5-20260101"
        );
        assert_eq!(
            model_cost(spec, &stored(None), CURRENT, &RATES, false),
            serving.then_some(LIST_PRICE)
        );
    }

    /// What was paid is what was paid, even for a model that prices to
    /// nothing today.
    #[test]
    fn a_recorded_cost_short_circuits_the_serving_rates() {
        assert_eq!(
            model_cost(
                UNRESOLVABLE,
                &stored(Some(RECORDED)),
                CURRENT,
                &RATES,
                false
            ),
            Some(RECORDED)
        );
    }

    #[test]
    fn stored_usage_add_keeps_a_recorded_cost_and_drops_a_lost_one() {
        let mut a = stored(Some(RECORDED));
        a += stored(None);
        assert_eq!(a.cost, Some(RECORDED));

        let mut b = stored(None);
        b += stored(None);
        assert_eq!(b.cost, None);

        let mut c = stored(Some(RECORDED));
        c += stored(Some(2.0 * RECORDED));
        assert_eq!(c.cost, Some(3.0 * RECORDED));
    }
}

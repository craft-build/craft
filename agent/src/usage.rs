//! Usage & cost accounting.
//!
//! A turn is priced once, when it runs, and that number is what gets stored,
//! summed and shown; history is never re-priced, because rates move. A
//! static [`PricedModel`] table holds the rates; unknown models are unpriced:
//! callers get `None` and show no cost instead of a made-up "$0.000".

use std::collections::HashMap;
use std::ops::AddAssign;

use serde::{Deserialize, Serialize};

const PER_MILLION: f64 = 1_000_000.0;

/// Token usage reported by the provider for one model call: animist's
/// counters, re-exported so every surface reads one type. Pricing stays
/// here because animist 0.7.0 leaves `cost` unset (followup F08 is open).
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

pub(crate) fn cost(usage: &TokenUsage, pricing: &ModelPricing, fast: bool) -> f64 {
    let (input, output, cache_write, cache_read) = match &pricing.fast {
        Some(f) if fast => (
            f.input,
            f.output,
            f.input * ModelPricing::CACHE_WRITE_MULTIPLIER,
            f.input * ModelPricing::CACHE_READ_MULTIPLIER,
        ),
        _ => (
            pricing.input,
            pricing.output,
            pricing.cache_write,
            pricing.cache_read,
        ),
    };
    usage.input as f64 * input / PER_MILLION
        + usage.output as f64 * output / PER_MILLION
        + usage.cache_creation as f64 * cache_write / PER_MILLION
        + usage.cache_read as f64 * cache_read / PER_MILLION
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

/// Per-million-token rates.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// A premium tier that differs per model. `None` means the model has no
    /// fast tier, so asking for fast mode quietly falls back to standard
    /// rates instead of overcharging.
    #[serde(default)]
    pub fast: Option<FastPricing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FastPricing {
    pub input: f64,
    pub output: f64,
}

impl ModelPricing {
    pub const ZERO: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
    };

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_write == 0.0 && self.cache_read == 0.0
    }

    const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
    const CACHE_READ_MULTIPLIER: f64 = 0.10;
}

/// A model and its rates: the registry-free model data that pricing
/// needs. `id` is the canonical entry name; `prefixes` match
/// dated snapshots without registry churn.
#[derive(Debug)]
pub struct PricedModel {
    pub provider: &'static str,
    pub id: &'static str,
    pub prefixes: &'static [&'static str],
    pub pricing: ModelPricing,
}

impl PricedModel {
    /// The quoted rates — the honest guess
    /// when re-pricing a session whose turns never recorded what they paid.
    ///
    /// `fast` arrives as the user's raw preference and is gated here,
    /// against *this* model; callers often price a model they are not
    /// running.
    pub fn list_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        let fast = fast && self.pricing.fast.is_some();
        (!self.pricing.is_zero()).then(|| cost(usage, &self.pricing, fast))
    }
}

/// Rates for the providers this repo talks to natively; everything else
/// is unpriced.
static PRICED_MODELS: &[PricedModel] = &[
    // Anthropic
    PricedModel {
        provider: "anthropic",
        id: "claude-haiku-4-5",
        prefixes: &["claude-haiku-4-5"],
        pricing: ModelPricing {
            input: 1.00,
            output: 5.00,
            cache_write: 1.25,
            cache_read: 0.10,
            fast: None,
        },
    },
    PricedModel {
        provider: "anthropic",
        id: "claude-sonnet-5",
        prefixes: &["claude-sonnet-5"],
        pricing: ModelPricing {
            input: 2.00,
            output: 10.00,
            cache_write: 2.50,
            cache_read: 0.20,
            fast: None,
        },
    },
    PricedModel {
        provider: "anthropic",
        id: "claude-opus-4-8",
        prefixes: &["claude-opus-4-8"],
        pricing: ModelPricing {
            input: 5.00,
            output: 25.00,
            cache_write: 6.25,
            cache_read: 0.50,
            fast: Some(FastPricing {
                input: 10.00,
                output: 50.00,
            }),
        },
    },
    PricedModel {
        provider: "anthropic",
        id: "claude-opus-5",
        prefixes: &["claude-opus-5"],
        pricing: ModelPricing {
            input: 5.00,
            output: 25.00,
            cache_write: 6.25,
            cache_read: 0.50,
            fast: Some(FastPricing {
                input: 10.00,
                output: 50.00,
            }),
        },
    },
    PricedModel {
        provider: "anthropic",
        id: "claude-fable-5",
        prefixes: &["claude-fable-5"],
        pricing: ModelPricing {
            input: 10.00,
            output: 50.00,
            cache_write: 12.50,
            cache_read: 1.00,
            fast: None,
        },
    },
    // OpenAI
    PricedModel {
        provider: "openai",
        id: "gpt-5.6-luna",
        prefixes: &["gpt-5.6-luna"],
        pricing: ModelPricing {
            input: 1.00,
            output: 6.00,
            cache_write: 1.25,
            cache_read: 0.10,
            fast: None,
        },
    },
    PricedModel {
        provider: "openai",
        id: "gpt-5.6-terra",
        prefixes: &["gpt-5.6-terra"],
        pricing: ModelPricing {
            input: 2.50,
            output: 15.00,
            cache_write: 3.125,
            cache_read: 0.25,
            fast: None,
        },
    },
    PricedModel {
        provider: "openai",
        id: "gpt-5.6-sol",
        prefixes: &["gpt-5.6-sol"],
        pricing: ModelPricing {
            input: 5.00,
            output: 30.00,
            cache_write: 6.25,
            cache_read: 0.50,
            fast: None,
        },
    },
    PricedModel {
        provider: "openai",
        id: "gpt-6-astra",
        prefixes: &["gpt-6-astra"],
        pricing: ModelPricing {
            input: 10.00,
            output: 50.00,
            cache_write: 12.50,
            cache_read: 1.00,
            fast: None,
        },
    },
    PricedModel {
        provider: "openai",
        id: "gpt-5.4-nano",
        prefixes: &["gpt-5.4-nano"],
        pricing: ModelPricing {
            input: 0.20,
            output: 1.25,
            cache_write: 0.00,
            cache_read: 0.02,
            fast: None,
        },
    },
    // DeepSeek
    PricedModel {
        provider: "deepseek",
        id: "deepseek-v4-flash",
        prefixes: &["deepseek-v4-flash"],
        pricing: ModelPricing {
            input: 0.22,
            output: 0.66,
            cache_write: 0.00,
            cache_read: 0.007,
            fast: None,
        },
    },
    PricedModel {
        provider: "deepseek",
        id: "deepseek-v4-pro",
        prefixes: &["deepseek-v4-pro"],
        pricing: ModelPricing {
            input: 0.66,
            output: 1.98,
            cache_write: 0.00,
            cache_read: 0.022,
            fast: None,
        },
    },
];

/// Resolve a `provider/model` spec against the static table, longest prefix
/// first so dated snapshots match without registry churn.
pub fn resolve_spec(spec: &str) -> Option<&'static PricedModel> {
    let (provider, model_id) = spec.split_once('/')?;
    resolve(provider, model_id)
}

/// Resolve a bare model id under a known provider slug.
pub fn resolve(provider: &str, model_id: &str) -> Option<&'static PricedModel> {
    PRICED_MODELS
        .iter()
        .filter(|m| m.provider == provider)
        .flat_map(|m| m.prefixes.iter().map(move |p| (p, m)))
        .filter(|(p, _)| model_id.starts_with(*p))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, m)| m)
}

/// The bill a session ran up; status bar, `/usage`, ACP and headless all
/// come here so they cannot disagree.
///
/// Turns record what they paid, so summing those is the truth. Counters
/// written before that kept no cost; their estimate against today's table is
/// settled into the entry once — after a later turn merges its own cost in,
/// the counters no longer say which of them was already paid for, so
/// estimating again would drop everything the entry had before. `None` when
/// nothing here is priced.
pub fn settle_session(
    total: &TokenUsage,
    by_model: &mut HashMap<String, StoredTokenUsage>,
    current_spec: &str,
    fast: bool,
) -> Option<f64> {
    if by_model.is_empty() && !is_zero(total) {
        by_model.insert(current_spec.to_owned(), billed(total, None));
    }
    for (spec, usage) in by_model.iter_mut() {
        usage.cost = model_cost(spec, usage, current_spec, fast);
    }
    by_model
        .values()
        .filter_map(|usage| usage.cost)
        .reduce(|total, cost| total + cost)
}

/// One model's slice of [`settle_session`], as `/usage` breaks it down per
/// row. Keys are `provider/model` specs, but a bare model id written by an
/// older era resolves against the current model's provider.
pub fn model_cost(
    spec: &str,
    usage: &StoredTokenUsage,
    current_spec: &str,
    fast: bool,
) -> Option<f64> {
    if let Some(cost) = usage.cost {
        return Some(cost);
    }
    let resolved = if let Some(model) = resolve_spec(spec) {
        Some(model)
    } else if !spec.contains('/') {
        let provider = current_spec.split_once('/').map_or("", |(p, _)| p);
        resolve(provider, spec)
    } else {
        None
    }?;
    resolved.list_cost(&(*usage).into(), fast)
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
        let pricing = ModelPricing {
            input: 1.0,
            output: 10.0,
            cache_write: 1.25,
            cache_read: 0.1,
            fast: None,
        };
        assert_eq!(cost(&usage, &pricing, false), 1.0 + 10.0 + 1.25 + 0.1);
    }

    #[test_case(false ; "standard_rates")]
    #[test_case(true ; "fast_rates")]
    fn fast_mode_replaces_the_table_rates(fast: bool) {
        let pricing = ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_write: 0.0,
            cache_read: 0.0,
            fast: Some(FastPricing {
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
            4.0 + 8.0 + 4.0 * 1.25 + 4.0 * 0.10
        } else {
            1.0 + 2.0
        };
        assert_eq!(cost(&usage, &pricing, fast), expected);
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

    #[test]
    fn unpriced_models_hide_their_cost() {
        let model = PricedModel {
            provider: "test",
            id: "free",
            prefixes: &["free"],
            pricing: ModelPricing::ZERO,
        };
        let usage = TokenUsage {
            input: MILLION,
            ..Default::default()
        };
        assert_eq!(model.list_cost(&usage, false), None);
    }

    #[test]
    fn dated_snapshots_resolve_by_prefix() {
        let model = resolve("anthropic", "claude-opus-5-20260101").expect("prefix match");
        assert_eq!(model.id, "claude-opus-5");
        assert!(resolve("anthropic", "no-such-model").is_none());
        assert_eq!(
            resolve_spec("deepseek/deepseek-v4-pro").map(|m| m.id),
            resolve("deepseek", "deepseek-v4-pro").map(|m| m.id)
        );
    }

    const CURRENT: &str = "anthropic/claude-sonnet-5";
    /// 1M input tokens at the current model's input rate (2.00).
    const LIST_PRICE: f64 = 2.0;
    const RECORDED: f64 = 0.5;
    const UNRESOLVABLE: &str = "test/a-model-no-table-has-ever-heard-of";
    const ALSO_UNRESOLVABLE: &str = "test/another-model-no-table-has-ever-heard-of";

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
    /// from the price table and [`RECORDED`] came from the turn itself. A
    /// breakdown nothing can price is not a free session: `Some(0.0)` would
    /// put a confident "$0.000" on screen.
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (ALSO_UNRESOLVABLE, Some(RECORDED))], Some(2.0 * RECORDED) ; "recorded_costs_win_over_the_price_table")]
    #[test_case(&[(CURRENT, None), (UNRESOLVABLE, None)], Some(LIST_PRICE)                                   ; "legacy_counters_use_each_models_own_rates")]
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
                settle_session(&total, &mut by_model, CURRENT, false),
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
            settle_session(&total, &mut by_model, CURRENT, false),
            Some(LIST_PRICE)
        );

        *by_model.entry(CURRENT.to_owned()).or_default() += stored(Some(RECORDED));

        assert_eq!(
            settle_session(&total, &mut by_model, CURRENT, false),
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
        assert_eq!(settle_session(&total, &mut by_model, unpriced, false), None);
        // Seeded with no price, like any other unresolvable entry.
        assert_eq!(by_model[unpriced].cost, None);
    }

    /// The sibling is priced differently on purpose, so passing here cannot be
    /// the current model's rates leaking through.
    #[test]
    fn bare_ids_resolve_against_the_current_provider() {
        let current = resolve_spec("anthropic/claude-sonnet-5").unwrap();
        let sibling_id = "claude-opus-5";
        let sibling = resolve("anthropic", sibling_id).unwrap();
        let usage = stored(None);

        let cost = model_cost(sibling_id, &usage, CURRENT, false);
        assert_eq!(cost, sibling.list_cost(&usage.into(), false));
        assert_ne!(cost, current.list_cost(&usage.into(), false));
    }

    /// What was paid is what was paid, even for a model that prices to
    /// nothing today.
    #[test]
    fn a_recorded_cost_short_circuits_the_price_table() {
        assert_eq!(
            model_cost(CURRENT, &stored(Some(RECORDED)), CURRENT, false),
            Some(RECORDED)
        );
    }

    #[test_case("anthropic/claude-opus-5" ; "fast_model")]
    #[test_case("anthropic/claude-sonnet-5" ; "model_without_a_fast_tier")]
    fn fast_preference_is_gated_per_model(spec: &str) {
        let model = resolve_spec(spec).unwrap();
        let usage = TokenUsage {
            input: MILLION,
            ..Default::default()
        };
        let standard = model.list_cost(&usage, false).unwrap();
        let fast = model.list_cost(&usage, true).unwrap();
        if model.pricing.fast.is_some() {
            assert!(fast > standard);
        } else {
            assert_eq!(fast, standard);
        }
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

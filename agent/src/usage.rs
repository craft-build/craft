//! Usage & cost accounting (H.6), ported from the reference
//! `craft-providers/src/{model.rs, pricing.rs}`.
//!
//! A turn is priced once, when it runs, and that number is what gets stored,
//! summed and shown. History is never re-priced, because rates move. The only
//! thing that moves them today is a provider's [`PricingSchedule`] (DeepSeek's
//! peak-hours surcharge), which scales the quoted off-peak table rates inside
//! its windows.
//!
//! The reference resolves rates through its model registry; until that lands
//! here (H.3), a static [`PricedModel`] table stands in. Unknown models are
//! unpriced: callers get `None` and show no cost instead of a made-up "$0.000".

use std::collections::HashMap;
use std::fmt;
use std::ops::AddAssign;

use jiff::Timestamp;
use jiff::civil::Weekday;
use jiff::tz::Offset;
use serde::{Deserialize, Serialize};

const HOURS_PER_DAY: u8 = 24;
const PER_MILLION: f64 = 1_000_000.0;
/// Multiplier of a provider that bills the same rate around the clock.
pub(crate) const FLAT_RATE: f64 = 1.0;

/// Token usage reported by the provider for one model call.
///
/// Cache fields are part of the accounting even though the Rig seam does not
/// surface them yet (they read 0 there); the pricing math keeps its shape so
/// the numbers are right the day they arrive.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens. Total input = `input + cache_read + cache_creation`.
    #[serde(rename = "input_tokens")]
    pub input: u32,
    #[serde(rename = "output_tokens")]
    pub output: u32,
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation: u32,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read: u32,
}

impl From<&crate::history::Usage> for TokenUsage {
    /// Convert the Rig-reported counters. Rig exposes no cache-token fields,
    /// so those stay zero until the SSE layer grows them.
    fn from(u: &crate::history::Usage) -> Self {
        Self {
            input: u32::try_from(u.input_tokens).unwrap_or(u32::MAX),
            output: u32::try_from(u.output_tokens).unwrap_or(u32::MAX),
            cache_creation: 0,
            cache_read: 0,
        }
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
    }
}

impl TokenUsage {
    /// Ready to store, with what the turn was billed. No `From<TokenUsage>` on
    /// purpose: a caller that forgets the cost quietly loses money from the
    /// session total, so saying it out loud is mandatory.
    pub fn billed(&self, cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: self.input,
            output: self.output,
            cache_creation: self.cache_creation,
            cache_read: self.cache_read,
            cost,
        }
    }

    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn context_tokens(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }

    pub fn format(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "")
    }

    /// Like [`format`](Self::format), but marks the cost as a running total.
    pub fn format_sum_cost(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "Σ")
    }

    fn format_cost(&self, cost: Option<f64>, prefix: &str) -> String {
        let tokens = format!(
            "{}\u{2191} {}\u{2193}",
            format_tokens(self.total_input()),
            format_tokens(self.output)
        );
        match cost {
            Some(cost) => format!("{tokens} {prefix}${cost:.3}"),
            None => tokens,
        }
    }

    /// Crate-private on purpose: pricing outside a priced model skips the
    /// provider's schedule.
    pub(crate) fn cost(&self, pricing: &ModelPricing, fast: bool) -> f64 {
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
        self.input as f64 * input / PER_MILLION
            + self.output as f64 * output / PER_MILLION
            + self.cache_creation as f64 * cache_write / PER_MILLION
            + self.cache_read as f64 * cache_read / PER_MILLION
    }
}

/// A [`TokenUsage`] plus what it was billed, ready for the session ledger.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    pub input: u32,
    pub output: u32,
    pub cache_creation: u32,
    pub cache_read: u32,
    /// What the turns that made up this entry actually paid, when they
    /// recorded it. `None` means the entry holds legacy counters only.
    pub cost: Option<f64>,
}

impl From<StoredTokenUsage> for TokenUsage {
    fn from(s: StoredTokenUsage) -> Self {
        Self {
            input: s.input,
            output: s.output,
            cache_creation: s.cache_creation,
            cache_read: s.cache_read,
        }
    }
}

impl AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        let cost = match (self.cost, rhs.cost) {
            (Some(a), Some(b)) => Some(a + b),
            (cost, None) | (None, cost) => cost,
        };
        let mut tokens = TokenUsage::from(*self);
        tokens += TokenUsage::from(rhs);
        *self = tokens.billed(cost);
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

/// Per-million-token rates, quoted off-peak.
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

/// Rates that move with the wall clock. DeepSeek doubles everything during its
/// peak UTC hours, so model tables quote the off-peak rates and the provider's
/// schedule scales the bill inside its windows.
///
/// One multiplier is enough while providers move all four rates together.
#[derive(Debug)]
pub struct PricingSchedule {
    windows: &'static [PricingWindow],
    weekdays_only: bool,
    multiplier: f64,
}

/// Half-open `[start, end)` in whole UTC hours. `start > end` wraps past
/// midnight, so a window never needs splitting in two.
#[derive(Debug)]
pub struct PricingWindow {
    start: u8,
    end: u8,
}

impl PricingWindow {
    /// `hours(22, 2)` wraps past midnight. Every window is a `const`, so a typo
    /// here is a build error rather than a mispriced turn.
    pub const fn hours(start: u8, end: u8) -> Self {
        assert!(start < HOURS_PER_DAY, "window starts after the day ends");
        assert!(end <= HOURS_PER_DAY, "window ends after the day ends");
        assert!(start != end, "window covers no time at all");
        Self { start, end }
    }

    fn contains(&self, hour: u8) -> bool {
        if self.start < self.end {
            hour >= self.start && hour < self.end
        } else {
            hour >= self.start || hour < self.end
        }
    }
}

/// `01:00-04:00`, the shape provider docs quote peak hours in.
impl fmt::Display for PricingWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:00-{:02}:00", self.start, self.end)
    }
}

impl PricingSchedule {
    pub const fn new(windows: &'static [PricingWindow], multiplier: f64) -> Self {
        assert!(
            !windows.is_empty(),
            "a schedule with no windows never applies; drop it instead"
        );
        assert!(
            multiplier > FLAT_RATE,
            "model tables quote the off-peak rates, so a schedule only ever adds a surcharge"
        );
        Self {
            windows,
            weekdays_only: false,
            multiplier,
        }
    }

    /// Keeps the surcharge off Saturday and Sunday, the way DeepSeek words its
    /// peak hours.
    pub const fn weekdays_only(mut self) -> Self {
        let mut i = 0;
        while i < self.windows.len() {
            assert!(
                self.windows[i].start < self.windows[i].end,
                "a wrapping window leaves its tail on the next day, which no run of weekdays can bill"
            );
            i += 1;
        }
        self.weekdays_only = true;
        self
    }

    /// DeepSeek publishes the days in UTC next to the hours, so the UTC
    /// weekday is the rule itself and not an approximation.
    pub(crate) fn multiplier_at(&self, at: Timestamp) -> f64 {
        let at = Offset::UTC.to_datetime(at);
        if self.weekdays_only && matches!(at.weekday(), Weekday::Saturday | Weekday::Sunday) {
            return FLAT_RATE;
        }
        if self.windows.iter().any(|w| w.contains(at.hour() as u8)) {
            self.multiplier
        } else {
            FLAT_RATE
        }
    }
}

/// `2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri`.
impl fmt::Display for PricingSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x during ", self.multiplier)?;
        for (i, window) in self.windows.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{window}")?;
        }
        f.write_str(" UTC")?;
        if self.weekdays_only {
            f.write_str(", Mon-Fri")?;
        }
        Ok(())
    }
}

/// A model and its rates: the registry-free slice of the reference `Model`
/// that pricing needs. `id` is the canonical entry name; `prefixes` match
/// dated snapshots without registry churn.
#[derive(Debug)]
pub struct PricedModel {
    pub provider: &'static str,
    pub id: &'static str,
    pub prefixes: &'static [&'static str],
    pub pricing: ModelPricing,
    /// Set by the providers whose rates move with the wall clock, so the hours
    /// sit next to the prices they scale. Everyone else bills flat.
    pub schedule: Option<&'static PricingSchedule>,
}

impl PricedModel {
    /// The quoted rates, with no wall-clock surcharge. Deterministic, which is
    /// what makes it right for re-pricing a session whose turns never recorded
    /// what they paid: the rate back then is unknown, and the table price is
    /// the honest guess.
    ///
    /// `fast` arrives as the user's raw preference and is gated here, against
    /// *this* model. Callers often price a model they are not running, so a
    /// gate on their side would answer for the wrong one.
    pub fn list_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        let fast = fast && self.pricing.fast.is_some();
        (!self.pricing.is_zero()).then(|| usage.cost(&self.pricing, fast))
    }

    /// What the provider charges right now, so it is only ever correct for a
    /// turn that just finished: under a schedule the answer moves with the
    /// clock. Anything historical wants [`Self::list_cost`].
    ///
    /// `None` on an unpriced model, so callers can hide the cost instead of
    /// showing a misleading "$0.000".
    pub fn billed_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        let cost = self.list_cost(usage, fast)?;
        Some(
            self.schedule
                .map_or(cost, |s| cost * s.multiplier_at(Timestamp::now())),
        )
    }

    /// [`Self::billed_cost`] at an explicit instant, for callers that know
    /// when the turn ran (and for tests, which need determinism).
    pub fn billed_cost_at(&self, usage: &TokenUsage, fast: bool, at: Timestamp) -> Option<f64> {
        let cost = self.list_cost(usage, fast)?;
        Some(self.schedule.map_or(cost, |s| cost * s.multiplier_at(at)))
    }
}

/// DeepSeek peak hours double every rate; the tables quote the off-peak ones.
/// The weekend stays off-peak around the clock.
/// <https://api-docs.deepseek.com/quick_start/pricing/>
const DEEPSEEK_PEAK_WINDOWS: &[PricingWindow] =
    &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
pub(crate) const DEEPSEEK_PEAK_HOURS: PricingSchedule =
    PricingSchedule::new(DEEPSEEK_PEAK_WINDOWS, 2.0).weekdays_only();

/// Rates copied from the reference v0.14.1 tables for the providers this repo
/// talks to natively. Everything else is unpriced until the model registry
/// (H.3) lands.
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
        schedule: None,
    },
    PricedModel {
        provider: "anthropic",
        id: "claude-sonnet-5",
        prefixes: &["claude-sonnet-5"],
        // Introductory rates until 2026-09-01, then 3.00 / 15.00 / 3.75 / 0.30.
        pricing: ModelPricing {
            input: 2.00,
            output: 10.00,
            cache_write: 2.50,
            cache_read: 0.20,
            fast: None,
        },
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: None,
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
        schedule: Some(&DEEPSEEK_PEAK_HOURS),
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
        schedule: Some(&DEEPSEEK_PEAK_HOURS),
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

/// The bill a session ran up. Status bar, `/usage`, ACP and headless all come
/// here, so they cannot disagree about the same session.
///
/// Turns record what they paid, so summing those is the truth. Counters
/// written before that kept no cost, and their estimate against today's table
/// is settled into the entry here, once: after a later turn merges its own
/// cost in, the counters no longer say which of them was already paid for, so
/// estimating again would drop everything the entry had before.
///
/// `None` when nothing here is priced, so callers show no cost instead of a
/// made up "$0.000".
pub fn settle_session(
    total: &TokenUsage,
    by_model: &mut HashMap<String, StoredTokenUsage>,
    current_spec: &str,
    fast: bool,
) -> Option<f64> {
    if by_model.is_empty() && *total != TokenUsage::default() {
        by_model.insert(current_spec.to_owned(), total.billed(None));
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

    const SECONDS_PER_MINUTE: i64 = 60;
    const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
    const SECONDS_PER_DAY: i64 = HOURS_PER_DAY as i64 * SECONDS_PER_HOUR;
    const PEAK: f64 = 2.0;
    const DAYS_SINCE_EPOCH: i64 = 20_000;
    const PEAK_WINDOWS: &[PricingWindow] =
        &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
    const WRAPPING_WINDOW: &[PricingWindow] = &[PricingWindow::hours(22, 2)];
    const UNTIL_MIDNIGHT_WINDOW: &[PricingWindow] = &[PricingWindow::hours(22, HOURS_PER_DAY)];
    const WHOLE_DAY_WINDOW: &[PricingWindow] = &[PricingWindow::hours(0, HOURS_PER_DAY)];
    const LAST_MINUTE: i64 = 59;
    const LAST_SECOND: i64 = 59;

    const MILLION: u32 = 1_000_000;
    /// Four counters that cannot be confused with each other.
    const COUNTERS: TokenUsage = TokenUsage {
        input: 11,
        output: 22,
        cache_creation: 33,
        cache_read: 44,
    };

    fn utc(day: i64, hour: i64, minute: i64, second: i64) -> Timestamp {
        Timestamp::from_second(
            day * SECONDS_PER_DAY + hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second,
        )
        .expect("timestamp in range")
    }

    // ---- pricing.rs ports -------------------------------------------------

    /// Windows are half-open down to the second, since an off-by-one bills a
    /// whole hour at the wrong rate. Every case also runs on a day before the
    /// epoch, where the timestamp is negative and must not wrap into another
    /// window.
    #[test_case(PEAK_WINDOWS, 0, LAST_MINUTE, LAST_SECOND, FLAT_RATE ; "last_second_before_the_start")]
    #[test_case(PEAK_WINDOWS, 1, 0, 0, PEAK                          ; "first_second_of_a_window")]
    #[test_case(PEAK_WINDOWS, 3, LAST_MINUTE, LAST_SECOND, PEAK      ; "last_second_inside")]
    #[test_case(PEAK_WINDOWS, 4, 0, 0, FLAT_RATE                     ; "first_second_after_the_end")]
    #[test_case(PEAK_WINDOWS, 7, 15, 0, PEAK                         ; "second_window")]
    #[test_case(PEAK_WINDOWS, 23, 0, 0, FLAT_RATE                    ; "after_the_last_window")]
    #[test_case(WRAPPING_WINDOW, 23, 0, 0, PEAK                      ; "wrapping_before_midnight")]
    #[test_case(WRAPPING_WINDOW, 0, 0, 0, PEAK                       ; "wrapping_across_midnight")]
    #[test_case(WRAPPING_WINDOW, 21, LAST_MINUTE, LAST_SECOND, FLAT_RATE ; "wrapping_start_is_half_open")]
    #[test_case(WRAPPING_WINDOW, 2, 0, 0, FLAT_RATE                  ; "wrapping_end_is_half_open")]
    #[test_case(UNTIL_MIDNIGHT_WINDOW, 23, LAST_MINUTE, LAST_SECOND, PEAK ; "ends_at_midnight")]
    #[test_case(UNTIL_MIDNIGHT_WINDOW, 0, 0, 0, FLAT_RATE            ; "ending_at_midnight_does_not_wrap")]
    #[test_case(WHOLE_DAY_WINDOW, 0, 0, 0, PEAK                      ; "whole_day_starts_at_midnight")]
    #[test_case(WHOLE_DAY_WINDOW, 23, LAST_MINUTE, LAST_SECOND, PEAK ; "whole_day_never_leaves_peak")]
    fn windows_price_by_the_utc_clock(
        windows: &'static [PricingWindow],
        hour: i64,
        minute: i64,
        second: i64,
        expected: f64,
    ) {
        let schedule = PricingSchedule::new(windows, PEAK);
        for day in [DAYS_SINCE_EPOCH, -DAYS_SINCE_EPOCH] {
            assert_eq!(
                schedule.multiplier_at(utc(day, hour, minute, second)),
                expected,
                "day {day}"
            );
        }
    }

    /// The weekend bills the very same hour a second way, so the day has to be
    /// read before the clock. `07:00` sits inside [`PEAK_WINDOWS`], `12:00`
    /// outside every one of them.
    #[test_case("2024-01-01T07:00:00Z", PEAK      ; "monday_inside_a_window")]
    #[test_case("2024-01-06T07:00:00Z", FLAT_RATE ; "saturday_inside_the_same_window")]
    #[test_case("2024-01-07T07:00:00Z", FLAT_RATE ; "sunday_inside_the_same_window")]
    #[test_case("2024-01-01T12:00:00Z", FLAT_RATE ; "monday_outside_every_window")]
    fn a_weekdays_only_schedule_leaves_the_weekend_alone(at: &str, expected: f64) {
        let schedule = PricingSchedule::new(PEAK_WINDOWS, PEAK).weekdays_only();
        let at: Timestamp = at.parse().expect("a valid timestamp");
        assert_eq!(schedule.multiplier_at(at), expected);
    }

    #[test]
    fn schedules_render_the_hours_and_days_they_bill() {
        assert_eq!(
            PricingSchedule::new(PEAK_WINDOWS, PEAK).to_string(),
            "2x during 01:00-04:00, 06:00-10:00 UTC"
        );
        assert_eq!(
            PricingSchedule::new(PEAK_WINDOWS, PEAK)
                .weekdays_only()
                .to_string(),
            "2x during 01:00-04:00, 06:00-10:00 UTC, Mon-Fri"
        );
        assert_eq!(
            PricingSchedule::new(WRAPPING_WINDOW, PEAK).to_string(),
            "2x during 22:00-02:00 UTC"
        );
    }

    // ---- model.rs ports ---------------------------------------------------

    #[test_case(999, "999"         ; "under_thousand")]
    #[test_case(1_000, "1.0k"      ; "thousand")]
    #[test_case(999_999, "1000.0k" ; "just_under_million")]
    #[test_case(1_000_000, "1.0m"  ; "million")]
    fn format_tokens_display(tokens: u32, expected: &str) {
        assert_eq!(format_tokens(tokens), expected);
    }

    #[test_case(TokenUsage { input: 12_000, output: 456, cache_creation: 200, cache_read: 100 }, None, "12.3k\u{2191} 456\u{2193}" ; "without_cost")]
    #[test_case(TokenUsage { input: 1_000_000, output: 100_000, cache_creation: 200_000, cache_read: 500_000 }, Some(5.4), "1.7m\u{2191} 100.0k\u{2193} $5.400" ; "with_cost")]
    #[test_case(TokenUsage { input: u32::MAX, output: 1, cache_creation: 1, cache_read: 1 }, None, "4295.0m\u{2191} 1\u{2193}" ; "input_saturates")]
    fn usage_formatting(usage: TokenUsage, cost: Option<f64>, expected: &str) {
        assert_eq!(usage.format(cost), expected);
    }

    #[test]
    fn each_counter_is_billed_at_its_own_rate() {
        let usage = TokenUsage {
            input: MILLION,
            output: MILLION,
            cache_creation: MILLION,
            cache_read: MILLION,
        };
        let pricing = ModelPricing {
            input: 1.0,
            output: 10.0,
            cache_write: 1.25,
            cache_read: 0.1,
            fast: None,
        };
        assert_eq!(usage.cost(&pricing, false), 1.0 + 10.0 + 1.25 + 0.1);
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
        };
        // Cache rates under fast mode derive from the fast input rate.
        let expected = if fast {
            4.0 + 8.0 + 4.0 * 1.25 + 4.0 * 0.10
        } else {
            1.0 + 2.0
        };
        assert_eq!(usage.cost(&pricing, fast), expected);
    }

    #[test]
    fn billed_keeps_counters_and_add_saturates() {
        let stored = COUNTERS.billed(Some(0.25));
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
        sum += COUNTERS;
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
            schedule: None,
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

    #[test_case("2026-01-05T07:00:00Z", 1.32 ; "monday_peak")]
    #[test_case("2026-01-03T07:00:00Z", 0.66 ; "saturday_off_peak")]
    fn deepseek_peaks_double_the_off_peak_table(at: &str, expected: f64) {
        let model = resolve("deepseek", "deepseek-v4-pro").unwrap();
        let usage = TokenUsage {
            input: MILLION,
            ..Default::default()
        };
        let at: Timestamp = at.parse().unwrap();
        assert_eq!(model.list_cost(&usage, false), Some(0.66));
        assert_eq!(model.billed_cost_at(&usage, false, at), Some(expected));
    }

    // ---- settle_session ---------------------------------------------------

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

    /// An entry written before turns recorded their cost used to keep only the
    /// cost of the next turn that touched it, so every later load reported the
    /// session as costing whatever it last did.
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
    fn rig_usage_converts_with_zero_cache_counters() {
        let rig = crate::history::Usage {
            input_tokens: 1_500,
            output_tokens: 300,
            total_tokens: 1_800,
        };
        assert_eq!(
            TokenUsage::from(&rig),
            TokenUsage {
                input: 1_500,
                output: 300,
                cache_creation: 0,
                cache_read: 0,
            }
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

//! User thinking preferences, provider-independent. Resolution to wire
//! fields belongs to animist's adapters; this module parses and projects
//! the preference (config, TUI/ACP pickers, session storage) and maps it
//! onto animist's [`ThinkingConfig`](animist::provider::ThinkingConfig) at
//! request time.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

pub const MIN_THINKING_BUDGET: u32 = 1024;
pub const THINKING_USAGE: &str =
    "Use off, adaptive, minimal, low, medium, high, xhigh, max, or a positive token budget";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn percent(self) -> u32 {
        match self {
            Self::Minimal => 10,
            Self::Low => 20,
            Self::Medium => 40,
            Self::High => 60,
            Self::XHigh => 80,
            Self::Max => 100,
        }
    }

    pub fn budget(self, max: u32) -> u32 {
        let max = max.max(MIN_THINKING_BUDGET);
        ((u64::from(max) * u64::from(self.percent()) / 100) as u32).clamp(MIN_THINKING_BUDGET, max)
    }

    pub fn from_budget(n: u32, max: u32) -> Self {
        let percent = u64::from(n) * 100 / u64::from(max.max(1));
        Self::ALL
            .into_iter()
            .find(|e| u64::from(e.percent()) >= percent)
            .unwrap_or(Self::Max)
    }

    /// Exact match, otherwise closest lower level, otherwise lowest supported.
    pub fn snap(self, supported: &[Self]) -> Self {
        if supported.is_empty() || supported.contains(&self) {
            return self;
        }
        supported
            .iter()
            .copied()
            .filter(|e| *e < self)
            .max()
            .unwrap_or_else(|| *supported.iter().min().unwrap())
    }

    fn from_animist(effort: animist::provider::Effort) -> Option<Self> {
        match effort {
            animist::provider::Effort::Minimal => Some(Self::Minimal),
            animist::provider::Effort::Low => Some(Self::Low),
            animist::provider::Effort::Medium => Some(Self::Medium),
            animist::provider::Effort::High => Some(Self::High),
            animist::provider::Effort::XHigh => Some(Self::XHigh),
            animist::provider::Effort::Max => Some(Self::Max),
            animist::provider::Effort::Adaptive => None,
        }
    }

    fn to_animist(self) -> animist::provider::Effort {
        match self {
            Self::Minimal => animist::provider::Effort::Minimal,
            Self::Low => animist::provider::Effort::Low,
            Self::Medium => animist::provider::Effort::Medium,
            Self::High => animist::provider::Effort::High,
            Self::XHigh => animist::provider::Effort::XHigh,
            Self::Max => animist::provider::Effort::Max,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| THINKING_USAGE.into())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThinkingConfig {
    #[default]
    Off,
    Adaptive,
    Effort(Effort),
    Budget(u32),
}

impl ThinkingConfig {
    pub fn is_enabled(self) -> bool {
        self != Self::Off
    }

    pub fn parse(input: &str, current: Self) -> Result<Self, String> {
        if input.trim().is_empty() {
            return Ok(if current.is_enabled() {
                Self::Off
            } else {
                Self::Adaptive
            });
        }
        input.trim().parse()
    }

    /// The full static list, used when no model metadata exists.
    pub fn choices() -> Vec<Self> {
        [Self::Off, Self::Adaptive]
            .into_iter()
            .chain(Effort::ALL.into_iter().map(Self::Effort))
            .collect()
    }

    pub fn cycle(self) -> Self {
        let choices = Self::choices();
        choices[(choices
            .iter()
            .position(|c| *c == self)
            .unwrap_or(choices.len() - 1)
            + 1)
            % choices.len()]
    }

    pub fn status_label(self) -> Option<String> {
        self.is_enabled().then(|| match self {
            Self::Adaptive => "thinking".into(),
            _ => format!("thinking: {self}"),
        })
    }

    /// The wire form animist's adapters resolve.
    pub fn to_animist(self) -> animist::provider::ThinkingConfig {
        match self {
            Self::Off => animist::provider::ThinkingConfig::Off,
            Self::Adaptive => animist::provider::ThinkingConfig::Adaptive,
            Self::Effort(effort) => animist::provider::ThinkingConfig::Effort(effort.to_animist()),
            Self::Budget(n) => animist::provider::ThinkingConfig::Budget(n),
        }
    }
}

impl FromStr for ThinkingConfig {
    type Err = String;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "off" => Ok(Self::Off),
            "adaptive" => Ok(Self::Adaptive),
            value => value.parse::<Effort>().map(Self::Effort).or_else(|_| {
                value
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .map(Self::Budget)
                    .ok_or_else(|| THINKING_USAGE.into())
            }),
        }
    }
}

impl fmt::Display for ThinkingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::Adaptive => f.write_str("adaptive"),
            Self::Effort(e) => e.fmt(f),
            Self::Budget(n) => n.fmt(f),
        }
    }
}

impl Serialize for ThinkingConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ThinkingConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(s) => s.parse().map_err(serde::de::Error::custom),
            Value::Bool(true) => Ok(Self::Adaptive),
            Value::Bool(false) => Ok(Self::Off),
            Value::Number(n) => n.to_string().parse().map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(THINKING_USAGE)),
        }
    }
}

/// The levels a picker offers for a model: off/adaptive always, the
/// efforts the model's metadata reports, and a saved budget stays
/// editable. `None` (no metadata) offers the full static list.
pub fn choices_for(
    current: ThinkingConfig,
    info: Option<&animist::provider::ModelInfo>,
) -> Vec<ThinkingConfig> {
    let Some(info) = info else {
        return ThinkingConfig::choices();
    };
    let thinking = &info.features.thinking;
    if !thinking.supported && !thinking.required {
        return vec![ThinkingConfig::Off];
    }
    let mut choices = Vec::new();
    if !thinking.required {
        choices.push(ThinkingConfig::Off);
    }
    choices.push(ThinkingConfig::Adaptive);
    let mut efforts: Vec<Effort> = thinking
        .efforts
        .iter()
        .filter_map(|effort| Effort::from_animist(effort.clone()))
        .collect();
    efforts.sort();
    efforts.dedup();
    choices.extend(efforts.into_iter().map(ThinkingConfig::Effort));
    // An explicitly entered token budget remains editable even when the
    // model reports only discrete efforts.
    if matches!(current, ThinkingConfig::Budget(_)) && !choices.contains(&current) {
        choices.push(current);
    }
    if choices.len() == 1 && matches!(choices[0], ThinkingConfig::Off) {
        return choices;
    }
    if choices.len() == 1 && choices[0] == ThinkingConfig::Adaptive && !thinking.required {
        // Optional thinking with no discrete efforts: off plus adaptive;
        // a required-reasoning model offers adaptive alone.
        choices.insert(0, ThinkingConfig::Off);
    }
    choices
}

/// The value a turn actually sends for a preference: [`reconcile_for`]
/// plus snapping to what the resolved model can express — an effort the
/// listing does not report snaps to the closest supported one, and a
/// token budget maps onto the nearest supported effort (effort-only
/// dialects reject explicit budgets), or `adaptive` when the model
/// reports no discrete efforts. The stored preference is never rewritten:
/// switching back to a model that supports it restores it.
pub fn effective_for(
    config: ThinkingConfig,
    info: Option<&animist::provider::ModelInfo>,
) -> ThinkingConfig {
    let config = reconcile_for(config, info);
    let Some(info) = info else {
        return config;
    };
    let thinking = &info.features.thinking;
    if !thinking.supported && !thinking.required {
        return ThinkingConfig::Off;
    }
    let efforts: Vec<Effort> = thinking
        .efforts
        .iter()
        .filter_map(|effort| Effort::from_animist(effort.clone()))
        .collect();
    match config {
        ThinkingConfig::Effort(effort) => {
            if efforts.is_empty() {
                // No discrete efforts to express: adaptive is the knob
                // every reasoning dialect accepts.
                ThinkingConfig::Adaptive
            } else {
                ThinkingConfig::Effort(effort.snap(&efforts))
            }
        }
        ThinkingConfig::Budget(budget) => {
            if efforts.is_empty() {
                ThinkingConfig::Adaptive
            } else {
                // Without a reported window the proportional mapping is
                // unknown; the lowest supported effort is the safe read.
                let window = info.config.context_window.unwrap_or(u32::MAX);
                ThinkingConfig::Effort(Effort::from_budget(budget, window).snap(&efforts))
            }
        }
        other => other,
    }
}

/// Capability reconciliation for session surfaces: a model that cannot
/// reason never keeps an enabled preference; a model that must reason
/// never keeps `off`. `None` leaves the preference untouched, like the
/// no-provider branch call sites already have.
pub fn reconcile_for(
    config: ThinkingConfig,
    info: Option<&animist::provider::ModelInfo>,
) -> ThinkingConfig {
    let Some(info) = info else {
        return config;
    };
    let thinking = &info.features.thinking;
    if thinking.required {
        if config == ThinkingConfig::Off {
            ThinkingConfig::Effort(Effort::Minimal)
        } else {
            config
        }
    } else if !thinking.supported {
        ThinkingConfig::Off
    } else {
        config
    }
}

/// Project a saved preference onto a filtered picker without changing the
/// preference used by subagents or other providers.
pub fn selected_choice(current: ThinkingConfig, choices: &[ThinkingConfig]) -> ThinkingConfig {
    if choices.contains(&current) {
        return current;
    }
    if let ThinkingConfig::Effort(effort) = current {
        let levels: Vec<_> = choices
            .iter()
            .filter_map(|choice| match choice {
                ThinkingConfig::Effort(level) => Some(*level),
                _ => None,
            })
            .collect();
        if !levels.is_empty() {
            return ThinkingConfig::Effort(effort.snap(&levels));
        }
    }
    choices
        .iter()
        .copied()
        .find(|choice| choice.is_enabled() == current.is_enabled())
        .or_else(|| choices.first().copied())
        .unwrap_or(ThinkingConfig::Off)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn efforts_info(
        efforts: &[animist::provider::Effort],
        supported: bool,
        required: bool,
    ) -> animist::provider::ModelInfo {
        let mut info = animist::provider::ModelInfo::default();
        info.ids = vec!["m".to_owned()];
        info.features.thinking = animist::provider::Thinking::builder()
            .supported(supported)
            .required(required)
            .efforts(efforts.to_vec())
            .build();
        info
    }

    #[test]
    fn effort_math_and_snapping() {
        assert_eq!(Effort::Low.budget(10_000), 2_000);
        // The budget never drops below the floor.
        assert_eq!(Effort::Minimal.budget(500), MIN_THINKING_BUDGET);
        assert_eq!(Effort::from_budget(2_000, 10_000), Effort::Low);
        assert_eq!(Effort::from_budget(9_999, 10_000), Effort::Max);
        // Exact match stays; unsupported snaps to the closest lower level,
        // else the lowest supported.
        assert_eq!(
            Effort::High.snap(&[Effort::Low, Effort::High]),
            Effort::High
        );
        assert_eq!(
            Effort::Max.snap(&[Effort::Low, Effort::Medium]),
            Effort::Medium
        );
        assert_eq!(Effort::Minimal.snap(&[Effort::Low]), Effort::Low);
    }

    #[test]
    fn parse_cycle_and_serialization() {
        for text in [
            "off", "adaptive", "minimal", "low", "medium", "high", "xhigh", "max", "4096",
        ] {
            let parsed: ThinkingConfig = text.parse().expect(text);
            assert_eq!(parsed.to_string(), text, "{text} round trips");
            let json = serde_json::to_string(&parsed).unwrap();
            assert_eq!(
                parsed,
                serde_json::from_str::<ThinkingConfig>(&json).unwrap()
            );
        }
        // Booleans deserialize to the toggle states.
        assert_eq!(
            serde_json::from_str::<ThinkingConfig>("true").unwrap(),
            ThinkingConfig::Adaptive
        );
        assert!("nonsense".parse::<ThinkingConfig>().is_err());
    }

    /// No metadata offers the full static list; a thinking-capable model
    /// lists off/adaptive plus its discrete efforts, and a saved budget
    /// stays editable even when the model reports only efforts.
    #[test]
    fn choices_follow_model_metadata() {
        assert_eq!(
            choices_for(ThinkingConfig::Off, None),
            ThinkingConfig::choices()
        );
        let info = efforts_info(
            &[
                animist::provider::Effort::Low,
                animist::provider::Effort::High,
            ],
            true,
            false,
        );
        assert_eq!(
            choices_for(ThinkingConfig::Off, Some(&info)),
            vec![
                ThinkingConfig::Off,
                ThinkingConfig::Adaptive,
                ThinkingConfig::Effort(Effort::Low),
                ThinkingConfig::Effort(Effort::High),
            ]
        );
        // A budget the model does not list remains selectable.
        assert!(
            choices_for(ThinkingConfig::Budget(4096), Some(&info))
                .contains(&ThinkingConfig::Budget(4096))
        );
        // Unsupported: off only. Required: no off.
        assert_eq!(
            choices_for(
                ThinkingConfig::Adaptive,
                Some(&efforts_info(&[], false, false))
            ),
            vec![ThinkingConfig::Off]
        );
        assert!(
            !choices_for(ThinkingConfig::Off, Some(&efforts_info(&[], true, true)))
                .contains(&ThinkingConfig::Off)
        );
    }

    #[test]
    fn reconcile_snaps_preference_to_capability() {
        // A required-reasoning model never keeps `off`.
        assert_eq!(
            reconcile_for(ThinkingConfig::Off, Some(&efforts_info(&[], true, true))),
            ThinkingConfig::Effort(Effort::Minimal)
        );
        // An incapable model never keeps an enabled preference.
        assert_eq!(
            reconcile_for(
                ThinkingConfig::Effort(Effort::High),
                Some(&efforts_info(&[], false, false))
            ),
            ThinkingConfig::Off
        );
        // No metadata leaves the preference untouched.
        assert_eq!(
            reconcile_for(ThinkingConfig::Effort(Effort::High), None),
            ThinkingConfig::Effort(Effort::High)
        );
    }

    #[test]
    fn selected_choice_projects_without_mutation() {
        let choices = vec![ThinkingConfig::Off, ThinkingConfig::Effort(Effort::Low)];
        assert_eq!(
            selected_choice(ThinkingConfig::Effort(Effort::High), &choices),
            ThinkingConfig::Effort(Effort::Low),
            "snaps to the closest supported effort"
        );
        assert_eq!(
            selected_choice(ThinkingConfig::Adaptive, &choices),
            ThinkingConfig::Effort(Effort::Low),
            "an enabled preference stays on the enabled axis"
        );
    }

    #[test]
    fn effective_for_snaps_to_expressible_levels() {
        let info = efforts_info(
            &[
                animist::provider::Effort::Low,
                animist::provider::Effort::High,
            ],
            true,
            false,
        );
        // A listed effort passes through; an unlisted one snaps down.
        assert_eq!(
            effective_for(ThinkingConfig::Effort(Effort::Low), Some(&info)),
            ThinkingConfig::Effort(Effort::Low)
        );
        assert_eq!(
            effective_for(ThinkingConfig::Effort(Effort::Max), Some(&info)),
            ThinkingConfig::Effort(Effort::High)
        );
        assert_eq!(
            effective_for(ThinkingConfig::Effort(Effort::Minimal), Some(&info)),
            ThinkingConfig::Effort(Effort::Low),
            "snaps up to the lowest supported when nothing lower exists"
        );
        // A budget maps onto the nearest supported effort.
        assert_eq!(
            effective_for(ThinkingConfig::Budget(2_000), Some(&info)),
            ThinkingConfig::Effort(Effort::Low)
        );
        assert_eq!(
            effective_for(ThinkingConfig::Budget(1024), Some(&info)),
            ThinkingConfig::Effort(Effort::Low)
        );
        // Off and adaptive are already expressible.
        assert_eq!(
            effective_for(ThinkingConfig::Off, Some(&info)),
            ThinkingConfig::Off
        );
        assert_eq!(
            effective_for(ThinkingConfig::Adaptive, Some(&info)),
            ThinkingConfig::Adaptive
        );
    }

    #[test]
    fn effective_for_degrades_budgets_and_efforts_without_listed_efforts() {
        let info = efforts_info(&[], true, false);
        // No discrete efforts: adaptive is the one knob every dialect
        // accepts, and a budget cannot be expressed as an effort.
        assert_eq!(
            effective_for(ThinkingConfig::Effort(Effort::High), Some(&info)),
            ThinkingConfig::Adaptive
        );
        assert_eq!(
            effective_for(ThinkingConfig::Budget(8192), Some(&info)),
            ThinkingConfig::Adaptive
        );
        assert_eq!(
            effective_for(ThinkingConfig::Adaptive, Some(&info)),
            ThinkingConfig::Adaptive
        );
        // No metadata at all: the preference passes through untouched.
        assert_eq!(
            effective_for(ThinkingConfig::Effort(Effort::Max), None),
            ThinkingConfig::Effort(Effort::Max)
        );
        // An incapable model never sends an enabled value (reconcile).
        assert_eq!(
            effective_for(
                ThinkingConfig::Effort(Effort::High),
                Some(&efforts_info(&[], false, false))
            ),
            ThinkingConfig::Off
        );
    }
}

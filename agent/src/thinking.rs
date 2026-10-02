//! User preferences are provider-independent. Only the DynamicModel boundary
//! resolves them to wire fields, including when a retry changes providers.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::providers::ProviderKind;

pub const MIN_THINKING_BUDGET: u32 = 1024;
const FALLBACK_MAX: u32 = 32_768;
const REQUEST_KEY: &str = "__craft_thinking";
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

/// models.dev and discovery declare knobs rather than assuming three levels.
/// Unknown option types remain readable and do not break catalog startup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningOption {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub values: Vec<String>,
    pub min: Option<u32>,
    pub max: Option<u32>,
}

/// Local chat templates may use arbitrary request fragments for each mode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThinkingFields {
    pub off: Option<Value>,
    pub adaptive: Option<Value>,
    #[serde(flatten)]
    pub levels: BTreeMap<Effort, Value>,
}

/// Per-model overrides take precedence over remote metadata and family defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelThinking {
    pub supports: Option<bool>,
    pub required: bool,
    pub options: Vec<ReasoningOption>,
    pub max_output: Option<u32>,
    pub fields: Option<ThinkingFields>,
}

pub fn model_info(
    kind: ProviderKind,
    id: &str,
    settings: Option<&crate::config::ModelConfig>,
) -> ModelThinking {
    let meta = crate::models_dev::metadata_for(catalog_key(kind), id);
    ModelThinking {
        supports: settings
            .and_then(|s| s.supports_thinking)
            .or_else(|| meta.as_ref().and_then(|m| m.supports_thinking)),
        required: settings.and_then(|s| s.requires_thinking).unwrap_or(false),
        options: settings
            .and_then(|s| s.reasoning_options.clone())
            .unwrap_or_else(|| {
                meta.as_ref()
                    .map(|m| m.reasoning_options.clone())
                    .unwrap_or_default()
            }),
        max_output: settings
            .and_then(|s| s.max_output_tokens)
            .or_else(|| meta.as_ref().map(|m| m.output)),
        fields: settings.and_then(|s| s.thinking_fields.clone()),
    }
}

/// Capability reconciliation is also used by session surfaces so an
/// unsupported model does not advertise an enabled thinking preference.
/// Effort snapping stays at request time, never chained across providers.
pub fn reconcile(
    config: ThinkingConfig,
    kind: ProviderKind,
    id: &str,
    info: &ModelThinking,
) -> ThinkingConfig {
    if info.required {
        if config == ThinkingConfig::Off {
            ThinkingConfig::Effort(Effort::Minimal)
        } else {
            config
        }
    } else if !info
        .supports
        .unwrap_or_else(|| info.fields.is_some() || known_support(kind, id))
    {
        ThinkingConfig::Off
    } else {
        config
    }
}

pub fn reconcile_for(
    config: ThinkingConfig,
    provider: &crate::config::ProviderConfig,
    id: &str,
) -> ThinkingConfig {
    reconcile(
        config,
        provider.kind,
        id,
        &model_info(provider.kind, id, provider.models.get(id)),
    )
}

pub fn catalog_key(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Gemini => "google",
        ProviderKind::Chatgpt => "openai",
        ProviderKind::Copilot => "github-copilot",
        ProviderKind::Moonshot => "moonshotai",
        _ => kind.as_str(),
    }
}

struct Dialect {
    levels: Vec<Effort>,
    off: bool,
    adaptive: Option<Effort>,
}

fn dialect(kind: ProviderKind, id: &str, options: &[ReasoningOption]) -> Dialect {
    use Effort::*;
    use ProviderKind::*;
    if let Some(option) = options.iter().find(|o| o.kind == "effort") {
        let mut levels: Vec<_> = option
            .values
            .iter()
            .filter_map(|v| v.parse().ok())
            .collect();
        levels.sort();
        levels.dedup();
        return Dialect {
            levels,
            off: option.values.iter().any(|s| s == "none"),
            adaptive: None,
        };
    }
    let (levels, off, adaptive) = match kind {
        Mistral => (vec![High], false, Some(High)),
        Deepseek => (vec![Max], false, None),
        Zai => (vec![High, XHigh], true, Some(High)),
        Xai => (vec![Low, Medium, High, XHigh], false, Some(High)),
        Anthropic | Bedrock => (vec![Low, Medium, High], false, None),
        Openai | Chatgpt | Azure | Copilot => {
            // Family fallback only when discovery is unavailable.
            if id.contains("gpt-5.2") || id.contains("gpt-5.3") || id.contains("gpt-5.4") {
                (vec![Low, Medium, High, XHigh], true, Some(Medium))
            } else if id.contains("gpt-5") {
                (vec![Minimal, Low, Medium, High], false, Some(Medium))
            } else {
                (vec![Low, Medium, High], false, Some(Medium))
            }
        }
        _ => (vec![Low, Medium, High], false, Some(High)),
    };
    Dialect {
        levels,
        off,
        adaptive,
    }
}

fn known_support(kind: ProviderKind, id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    match kind {
        ProviderKind::Cohere | ProviderKind::Voyageai => false,
        ProviderKind::Anthropic | ProviderKind::Bedrock => {
            id.contains("claude") && (id.contains("3-7") || id.contains("-4") || id.contains("-5"))
        }
        ProviderKind::Openai | ProviderKind::Azure | ProviderKind::Chatgpt => {
            id.starts_with("gpt-5") || ["o1", "o3", "o4"].iter().any(|p| id.starts_with(p))
        }
        ProviderKind::Gemini => {
            id.contains("2.5") || id.contains("gemini-3") || id.contains("latest")
        }
        ProviderKind::Mistral => id.contains("magistral"),
        ProviderKind::Xai => id.contains("grok-3-mini") || id.contains("grok-4"),
        ProviderKind::Deepseek | ProviderKind::Zai => true,
        // For custom/local gateways, an explicit preference remains usable
        // without an online catalog. Off sends no unsupported effort field.
        _ => true,
    }
}

pub(crate) fn uses_adaptive_claude(id: &str) -> bool {
    let bare = id.rsplit('/').next().unwrap_or(id);
    let bare = bare.split("claude-").nth(1).unwrap_or("");
    let mut parts = bare.split(['-', '.']);
    let family = parts.next().unwrap_or("");
    let major = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0);
    (major, minor) >= if family == "opus" { (4, 7) } else { (5, 0) }
}

fn budget(config: ThinkingConfig, ceiling: Option<u32>, floor: u32) -> Option<u32> {
    let n = match config {
        ThinkingConfig::Budget(n) => n,
        ThinkingConfig::Effort(e) => e.budget(ceiling.unwrap_or(FALLBACK_MAX)),
        _ => return None,
    };
    Some(match ceiling {
        Some(max) => n.clamp(floor, max.max(floor)),
        None => n.max(floor),
    })
}

/// Deep merge preserves unrelated caller parameters (sampling, schemas, etc.).
pub fn merge(target: &mut Value, extra: Value) {
    match (target, extra) {
        (Value::Object(target), Value::Object(extra)) => {
            for (key, value) in extra {
                merge(target.entry(key).or_insert(Value::Null), value);
            }
        }
        (target, extra) => *target = extra,
    }
}

/// Encode only at the provider boundary. Output caps constrain the thinking
/// budget too: half the effective output window is reserved for the answer.
pub fn wire(
    config: ThinkingConfig,
    kind: ProviderKind,
    id: &str,
    info: &ModelThinking,
    request_cap: Option<u64>,
) -> Value {
    use ThinkingConfig::*;
    let supports = info
        .supports
        .unwrap_or_else(|| info.fields.is_some() || known_support(kind, id));
    if !supports && !info.required {
        return json!({});
    }
    let config = reconcile(config, kind, id, info);
    let output = match (info.max_output.map(u64::from), request_cap) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let mut ceiling = output.map(|n| (n / 2).min(u64::from(u32::MAX)) as u32);
    let budget_option = info.options.iter().find(|o| o.kind == "budget_tokens");
    if let Some(max) = budget_option.and_then(|o| o.max) {
        ceiling = Some(ceiling.map_or(max, |n| n.min(max)));
    }
    let floor = budget_option
        .and_then(|o| o.min)
        .unwrap_or(MIN_THINKING_BUDGET);
    let d = dialect(kind, id, &info.options);
    let effort = match config {
        Off => {
            if d.off {
                Some("none")
            } else {
                // Effort-only models that advertise no disabling value still
                // need an explicit minimum, rather than an expensive API default.
                info.options
                    .iter()
                    .any(|o| o.kind == "effort")
                    .then(|| d.levels.first().map(|e| e.as_str()))
                    .flatten()
            }
        }
        Adaptive => d.adaptive.map(|e| e.as_str()),
        Effort(e) => (!d.levels.is_empty()).then(|| e.snap(&d.levels).as_str()),
        Budget(n) => (!d.levels.is_empty()).then(|| {
            crate::thinking::Effort::from_budget(n, ceiling.unwrap_or(FALLBACK_MAX))
                .snap(&d.levels)
                .as_str()
        }),
    };

    if let Some(fields) = &info.fields {
        let fragment = match config {
            Off => fields.off.as_ref(),
            Adaptive => fields.adaptive.as_ref(),
            Effort(e) => fields
                .levels
                .get(&e.snap(&fields.levels.keys().copied().collect::<Vec<_>>())),
            Budget(n) => fields.levels.get(
                &crate::thinking::Effort::from_budget(n, ceiling.unwrap_or(FALLBACK_MAX))
                    .snap(&fields.levels.keys().copied().collect::<Vec<_>>()),
            ),
        };
        let fragment = fragment.or_else(|| {
            config
                .is_enabled()
                .then_some(fields.adaptive.as_ref())
                .flatten()
        });
        if let Some(fragment) = fragment {
            let mut fragment = fragment.clone();
            if matches!(config, Budget(_)) && fields.levels.is_empty() {
                merge(
                    &mut fragment,
                    json!({"thinking_budget_tokens": budget(config, ceiling, floor)}),
                );
            }
            return fragment;
        }
    }

    match kind {
        ProviderKind::Bedrock if !id.contains("claude") => {
            // Converse hosts unrelated model families. They must not receive
            // Claude parameters; custom thinking_fields can define their knobs.
            if config.is_enabled() {
                tracing::warn!(
                    model = id,
                    "Bedrock thinking controls require a Claude model or configured thinking_fields"
                );
            }
            json!({})
        }
        ProviderKind::Anthropic | ProviderKind::Bedrock => {
            if config == Off {
                return json!({});
            }
            if uses_adaptive_claude(id) {
                let mut body = json!({"thinking": {"type": "adaptive", "display": "summarized"}});
                if let Some(effort) = effort {
                    body["output_config"] = json!({"effort": effort});
                }
                body
            } else {
                match budget(config, ceiling, floor.max(MIN_THINKING_BUDGET)) {
                    Some(n) => json!({"thinking": {"type": "enabled", "budget_tokens": n}}),
                    None => json!({"thinking": {"type": "enabled", "budget_tokens":
                        crate::thinking::Effort::High.budget(ceiling.unwrap_or(FALLBACK_MAX))}}),
                }
            }
        }
        ProviderKind::Gemini => {
            let has_levels =
                info.options.iter().any(|o| o.kind == "effort") || id.contains("gemini-3");
            let thinking = if has_levels {
                // Rig's native enum accepts exactly these four levels.
                let levels = [
                    crate::thinking::Effort::Minimal,
                    crate::thinking::Effort::Low,
                    crate::thinking::Effort::Medium,
                    crate::thinking::Effort::High,
                ];
                let level = match config {
                    _ if d.levels.is_empty() => None,
                    Off => Some(
                        crate::thinking::Effort::Minimal
                            .snap(&d.levels)
                            .snap(&levels),
                    ),
                    Effort(e) => Some(e.snap(&d.levels).snap(&levels)),
                    Budget(n) => Some(
                        crate::thinking::Effort::from_budget(n, ceiling.unwrap_or(FALLBACK_MAX))
                            .snap(&d.levels)
                            .snap(&levels),
                    ),
                    Adaptive => None,
                };
                match level {
                    Some(level) => {
                        json!({"thinkingLevel": level.as_str(), "includeThoughts": config != Off})
                    }
                    None => json!({"includeThoughts": config != Off}),
                }
            } else {
                let cap = if id.contains("flash") { 24_576 } else { 32_768 };
                let ceiling = Some(ceiling.map_or(cap, |n| n.min(cap)));
                match config {
                    Off => json!({"thinkingBudget": budget_option.and_then(|o| o.min)
                        .unwrap_or(if id.contains("pro") { 128 } else { 0 })}),
                    Adaptive => json!({"includeThoughts": true}),
                    _ => {
                        json!({"thinkingBudget": budget(config, ceiling, floor), "includeThoughts": true})
                    }
                }
            };
            json!({"generationConfig": {"thinkingConfig": thinking}})
        }
        ProviderKind::Ollama => {
            // Ollama distinguishes level-controlled GPT-OSS from toggle models.
            let has_levels =
                id.contains("gpt-oss") || info.options.iter().any(|o| o.kind == "effort");
            match config {
                Off => json!({"think": false}),
                Adaptive => json!({"think": true}),
                _ if has_levels && !d.levels.is_empty() => {
                    let levels = [
                        crate::thinking::Effort::Low,
                        crate::thinking::Effort::Medium,
                        crate::thinking::Effort::High,
                        crate::thinking::Effort::Max,
                    ];
                    let e: crate::thinking::Effort = effort
                        .unwrap_or("high")
                        .parse()
                        .unwrap_or(crate::thinking::Effort::High);
                    json!({"think": e.snap(&levels).as_str()})
                }
                _ => json!({"think": true}),
            }
        }
        ProviderKind::Deepseek | ProviderKind::Zai => {
            let mut body =
                json!({"thinking": {"type": if config == Off { "disabled" } else { "enabled" }}});
            let declares_effort = info.options.iter().any(|o| o.kind == "effort");
            if config != Off
                && (kind == ProviderKind::Deepseek || declares_effort)
                && let Some(effort) = effort
            {
                body["reasoning_effort"] = json!(effort);
            }
            body
        }
        ProviderKind::Chatgpt
        | ProviderKind::Xai
        | ProviderKind::Openai
        | ProviderKind::Openrouter => {
            // rig openai::Client defaults to Responses, not CompletionsClient.
            effort.map_or_else(|| json!({}), |e| json!({"reasoning": {"effort": e}}))
        }
        ProviderKind::Copilot if id.to_ascii_lowercase().contains("codex") => {
            effort.map_or_else(|| json!({}), |e| json!({"reasoning": {"effort": e}}))
        }
        ProviderKind::Llamafile => {
            let n = match config {
                Off => 0_i64,
                Adaptive => -1,
                _ => i64::from(budget(config, ceiling, floor).unwrap()),
            };
            json!({"thinking_budget_tokens": n})
        }
        ProviderKind::Cohere | ProviderKind::Voyageai => json!({}),
        _ => effort.map_or_else(|| json!({}), |e| json!({"reasoning_effort": e})),
    }
}

/// Internal request envelope. It is removed before invoking rig; keeping raw
/// preferences here lets retry/fallback handles choose their own dialect.
pub fn attach(request: &mut rig_core::completion::CompletionRequest, thinking: ThinkingConfig) {
    let params = request.additional_params.get_or_insert_with(|| json!({}));
    params[REQUEST_KEY] = json!(thinking);
}

pub fn take(
    request: &mut rig_core::completion::CompletionRequest,
) -> Result<ThinkingConfig, String> {
    let value = request
        .additional_params
        .as_mut()
        .and_then(Value::as_object_mut)
        .and_then(|map| map.remove(REQUEST_KEY));
    match value {
        Some(value) => serde_json::from_value(value).map_err(|e| e.to_string()),
        None => Ok(ThinkingConfig::Off),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cycle_and_serialization() {
        for c in ThinkingConfig::choices()
            .into_iter()
            .chain([ThinkingConfig::Budget(4096)])
        {
            assert_eq!(c.to_string().parse::<ThinkingConfig>().unwrap(), c);
            assert_eq!(
                serde_json::from_value::<ThinkingConfig>(json!(c)).unwrap(),
                c
            );
        }
        for value in ["0", "-1", "ultra", ""] {
            assert!(value.parse::<ThinkingConfig>().is_err());
        }
        assert_eq!(
            ThinkingConfig::parse("", ThinkingConfig::Off).unwrap(),
            ThinkingConfig::Adaptive
        );
        assert_eq!(
            ThinkingConfig::Effort(Effort::Max).cycle(),
            ThinkingConfig::Off
        );
    }

    #[test]
    fn effort_math_and_snapping() {
        assert_eq!(Effort::Low.budget(32_768), 6553);
        assert_eq!(Effort::Minimal.budget(1024), 1024);
        assert_eq!(
            Effort::XHigh.snap(&[Effort::Low, Effort::High]),
            Effort::High
        );
        assert_eq!(Effort::Minimal.snap(&[Effort::High]), Effort::High);
        assert_eq!(
            budget(ThinkingConfig::Budget(999_999), Some(16_384), 1024),
            Some(16_384)
        );
        assert_eq!(
            budget(ThinkingConfig::Budget(999_999), None, 1024),
            Some(999_999)
        );
    }

    #[test]
    fn adaptive_and_budget_claude() {
        let info = ModelThinking {
            max_output: Some(64_000),
            ..Default::default()
        };
        let c = ThinkingConfig::Budget(90_000);
        assert_eq!(
            wire(
                c,
                ProviderKind::Anthropic,
                "claude-sonnet-4-5",
                &info,
                Some(4096)
            )["thinking"]["budget_tokens"],
            2048
        );
        assert_eq!(
            wire(
                ThinkingConfig::Effort(Effort::Low),
                ProviderKind::Anthropic,
                "anthropic/claude-opus-4-7",
                &info,
                None
            )["output_config"]["effort"],
            "low"
        );
    }

    #[test]
    fn per_model_levels_and_explicit_off() {
        let info = ModelThinking {
            supports: Some(true),
            options: vec![ReasoningOption {
                kind: "effort".into(),
                values: vec!["none".into(), "low".into(), "xhigh".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            wire(
                ThinkingConfig::Effort(Effort::High),
                ProviderKind::Openai,
                "gpt-5.2",
                &info,
                None
            ),
            json!({"reasoning": {"effort": "low"}})
        );
        assert_eq!(
            wire(
                ThinkingConfig::Off,
                ProviderKind::Openai,
                "gpt-5.2",
                &info,
                None
            ),
            json!({"reasoning": {"effort": "none"}})
        );
        assert_eq!(
            wire(
                ThinkingConfig::Off,
                ProviderKind::Zai,
                "glm-4.6",
                &info,
                None
            ),
            json!({"thinking": {"type": "disabled"}})
        );
    }

    #[test]
    fn unsupported_models_send_no_controls() {
        let info = ModelThinking {
            supports: Some(false),
            ..Default::default()
        };
        assert_eq!(
            wire(
                ThinkingConfig::Effort(Effort::High),
                ProviderKind::Openai,
                "gpt-4o",
                &info,
                None
            ),
            json!({})
        );
    }

    #[test]
    fn gemini_and_local_shapes() {
        let info = ModelThinking {
            supports: Some(true),
            ..Default::default()
        };
        assert_eq!(
            wire(
                ThinkingConfig::Off,
                ProviderKind::Gemini,
                "gemini-2.5-flash",
                &info,
                None
            )["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
        assert_eq!(
            wire(
                ThinkingConfig::Off,
                ProviderKind::Ollama,
                "qwen3",
                &info,
                None
            ),
            json!({"think": false})
        );
        assert_eq!(
            wire(
                ThinkingConfig::Effort(Effort::Low),
                ProviderKind::Xai,
                "grok-4.6",
                &info,
                None
            ),
            json!({"reasoning": {"effort": "low"}})
        );
    }

    #[test]
    fn local_adaptive_fragment_keeps_explicit_budget() {
        let info = ModelThinking {
            fields: Some(ThinkingFields {
                adaptive: Some(json!({"chat_template_kwargs": {"enable_thinking": true}})),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            wire(
                ThinkingConfig::Budget(4096),
                ProviderKind::Llamafile,
                "local",
                &info,
                None
            ),
            json!({"chat_template_kwargs": {"enable_thinking": true}, "thinking_budget_tokens": 4096})
        );
        assert_eq!(
            wire(
                ThinkingConfig::Effort(Effort::Low),
                ProviderKind::Llamafile,
                "local",
                &info,
                None
            ),
            json!({"chat_template_kwargs": {"enable_thinking": true}})
        );
    }

    #[test]
    fn required_support_wins_and_non_claude_bedrock_gets_no_claude_fields() {
        let info = ModelThinking {
            supports: Some(false),
            required: true,
            ..Default::default()
        };
        assert_eq!(
            reconcile(
                ThinkingConfig::Off,
                ProviderKind::Openai,
                "mandatory",
                &info
            ),
            ThinkingConfig::Effort(Effort::Minimal)
        );
        assert_eq!(
            reconcile(
                ThinkingConfig::Effort(Effort::High),
                ProviderKind::Openai,
                "mandatory",
                &info
            ),
            ThinkingConfig::Effort(Effort::High)
        );
        let info = ModelThinking {
            supports: Some(true),
            ..Default::default()
        };
        assert_eq!(
            wire(
                ThinkingConfig::Budget(4096),
                ProviderKind::Bedrock,
                "amazon.nova-2-pro",
                &info,
                None
            ),
            json!({})
        );
    }

    #[test]
    fn unknown_declared_efforts_never_fall_back_to_unadvertised_levels() {
        let info = ModelThinking {
            supports: Some(true),
            options: vec![ReasoningOption {
                kind: "effort".into(),
                values: vec!["none".into(), "default".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            wire(
                ThinkingConfig::Effort(Effort::High),
                ProviderKind::Openai,
                "future",
                &info,
                None
            ),
            json!({})
        );
        assert_eq!(
            wire(
                ThinkingConfig::Off,
                ProviderKind::Openai,
                "future",
                &info,
                None
            ),
            json!({"reasoning": {"effort": "none"}})
        );
    }
}

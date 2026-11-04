//! Configuration for `~/.config/craft.bml` or the split files under
//! `~/.config/craft/*.bml` (legacy `~/.craft/` is searched first when it
//! exists). Format is BarkML.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;
use snafu::ResultExt;

use crate::error::{
    InvalidBaseUrlSnafu, InvalidBmlSnafu, InvalidProviderSnafu, InvalidSnafu, LoadConfigSnafu,
    ReadConfigSnafu, Result,
};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agent: AgentConfig,
    /// Explicit thinking preference. When set, it overrides the level restored
    /// from the last session on a clean launch, and it seeds new sessions;
    /// saved session preferences win on resume.
    pub always_thinking: Option<crate::thinking::ThinkingConfig>,
    /// Compaction stages, ascending by context fill ratio.
    #[serde(default = "default_compaction")]
    pub compaction: Vec<CompactionConfig>,
    /// Context reserved so compaction fires before the window truly fills.
    #[serde(default = "default_compaction_buffer")]
    pub compaction_buffer: CompactionBuffer,
    /// Tool-output pre-compression applied to the model's request view.
    pub compression: crate::compression::CompressionConfig,
    /// User keybinding overlay: snake_case action id → chord list.
    /// An empty list disables the action; unknown ids are warned and dropped.
    #[serde(default)]
    pub keybindings: std::collections::BTreeMap<String, Vec<String>>,
    /// OS command sandbox for `bash` (fail-closed when the backend is
    /// required but missing). Defaults preserve the shipped behavior:
    /// enabled, `workspace_write`, network allowed.
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

/// `[sandbox]` section, resolved into a [`crate::sandbox::SandboxPolicy`]
/// at session start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: bool,
    /// `workspace_write` | `read_only` | `danger_full_access` | `off`.
    pub mode: crate::sandbox::SandboxMode,
    pub network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: crate::sandbox::SandboxMode::WorkspaceWrite,
            network: true,
        }
    }
}

/// Which compaction strategy runs when a stage's threshold is crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionKind {
    /// LLM-powered summary of the conversation head.
    Llm,
    /// Deterministic no-LLM summary.
    Vcc,
}

/// One `[[compaction]]` stage: run `kind` once history reaches `context`
/// (a fill ratio of the model's context window, 0-1).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    pub kind: CompactionKind,
    /// Accepts a number or a quoted string ("0.8").
    #[serde(deserialize_with = "de_ratio")]
    pub context: f64,
}

fn de_ratio<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Ratio {
        Number(f64),
        String(String),
    }
    match Ratio::deserialize(deserializer)? {
        Ratio::Number(value) => Ok(value),
        Ratio::String(text) => text
            .trim()
            .parse()
            .map_err(|_| serde::de::Error::custom("context must be a ratio between 0 and 1")),
    }
}

/// Context reserved for compaction: an absolute token count or a percent of
/// the context window (BML: `20000` or `"20%"`). Subtracted from the window
/// when the engine decides whether the context is full, so compaction fires
/// before the provider would start rejecting requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBuffer {
    Tokens(u32),
    Percent(u8),
}

/// Tokens below this the buffer is pointless: overflow would arrive before
/// compaction could react.
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);

fn default_compaction_buffer() -> CompactionBuffer {
    DEFAULT_COMPACTION_BUFFER
}

impl Default for CompactionBuffer {
    fn default() -> Self {
        DEFAULT_COMPACTION_BUFFER
    }
}

impl CompactionBuffer {
    /// Resolve against a context window: percent scales with the window,
    /// token counts are absolute.
    pub fn resolve(self, context_window: u32) -> u32 {
        match self {
            Self::Tokens(n) => n,
            Self::Percent(p) => (u64::from(context_window) * u64::from(p) / 100) as u32,
        }
    }
}

impl<'de> Deserialize<'de> for CompactionBuffer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = CompactionBuffer;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a token count (>= 1000) or a percent of the window like \"20%\"")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v).map_err(|_| E::custom("compaction_buffer must be nonnegative"))?;
                self.visit_u64(v as u64)
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u32::try_from(v)
                    .ok()
                    .filter(|n| *n >= MIN_COMPACTION_BUFFER)
                    .map(CompactionBuffer::Tokens)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "compaction_buffer must be at least {MIN_COMPACTION_BUFFER} tokens"
                        ))
                    })
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let Some(digits) = v.strip_suffix('%') else {
                    return Err(E::custom("expected a trailing '%' for a percent buffer"));
                };
                let percent: u8 = digits.trim().parse().map_err(|_| {
                    E::custom(format!(
                        "invalid compaction_buffer {v:?}: expected a percent 0-100"
                    ))
                })?;
                Ok(CompactionBuffer::Percent(percent))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

fn default_compaction() -> Vec<CompactionConfig> {
    vec![
        CompactionConfig {
            kind: CompactionKind::Vcc,
            context: 0.6,
        },
        CompactionConfig {
            kind: CompactionKind::Llm,
            context: 0.8,
        },
    ]
}

/// Advisor auto-act severity threshold. Declaration order doubles as
/// the severity ranking: `Off < Nit < Concern < Blocker`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AdvisorAutoAct {
    #[default]
    Off,
    Nit,
    Concern,
    Blocker,
}

/// Always-on lightweight reviewer that reads the transcript delta after a
/// terminal reply and emits at most one deduped note. Off by default.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Maximum advisor notes kept in the dedup FIFO.
    #[serde(default = "default_advisor_dedup_size")]
    pub dedup_size: usize,
    /// Minimum severity that triggers an automatic follow-up turn instead of
    /// stopping for the user. Notes at or above this severity are pushed into
    /// the agent's own context and the run continues.
    #[serde(default = "default_advisor_auto_act")]
    pub auto_act: AdvisorAutoAct,
    /// Maximum advisor-driven follow-up turns a single run may take.
    #[serde(default = "default_advisor_max_act_turns")]
    pub max_act_turns: u32,
}

fn default_advisor_dedup_size() -> usize {
    8
}

fn default_advisor_auto_act() -> AdvisorAutoAct {
    AdvisorAutoAct::Concern
}

fn default_advisor_max_act_turns() -> u32 {
    2
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dedup_size: default_advisor_dedup_size(),
            auto_act: default_advisor_auto_act(),
            max_act_turns: default_advisor_max_act_turns(),
        }
    }
}

/// Defaults for each agent run, independent of provider/model selection.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// System instructions. An empty string keeps the built-in defaults; any
    /// text is appended to the assembled system prompt's instructions slot.
    pub preamble: String,
    /// Omit to preserve the provider/model's default sampling behavior.
    pub temperature: Option<f64>,
    /// Request-level output cap, not a model catalog metadata override.
    pub max_tokens: Option<u64>,
    /// Run default and inherited subagent preference.
    pub thinking: Option<crate::thinking::ThinkingConfig>,
    /// Bound on model calls per run. `None` keeps each surface's default
    /// (unbounded in the TUI; the headless builder defaults it);
    /// set by `--max-turns`.
    pub max_turns: Option<u32>,
    /// Bound on tools executing concurrently within one wave; it bounds both
    /// model-emitted calls and `batch` children (06's scheduling claims).
    #[serde(default = "default_max_concurrent_tools")]
    pub max_concurrent_tools: usize,
    /// Post-turn advisor.
    pub advisor: AdvisorConfig,
    /// System prompt rendering for every prompt in the session; fixed at
    /// conversation build.
    #[serde(default)]
    pub prompt_format: crate::prompt::PromptFormat,
    /// Post-turn argosy memory extraction: durable facts from the turn
    /// are written into the project's local argosy after a successful run.
    /// Default on.
    #[serde(default = "default_memory_extraction")]
    pub memory_extraction: bool,
}

fn default_memory_extraction() -> bool {
    true
}

fn default_max_concurrent_tools() -> usize {
    8
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            preamble: String::new(),
            temperature: None,
            max_tokens: None,
            thinking: None,
            max_turns: None,
            max_concurrent_tools: default_max_concurrent_tools(),
            advisor: AdvisorConfig::default(),
            prompt_format: crate::prompt::PromptFormat::default(),
            memory_extraction: default_memory_extraction(),
        }
    }
}

impl AgentConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_tokens == Some(0) {
            return InvalidSnafu {
                reason: "agent.max_tokens must be positive",
            }
            .fail();
        }
        if self.max_turns == Some(0) {
            return InvalidSnafu {
                reason: "agent.max_turns must be positive",
            }
            .fail();
        }
        // Providers have different upper bounds (and some disallow temperature).
        // Validate the portable constraint here; providers enforce their own bounds.
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return InvalidSnafu {
                reason: "agent.temperature must be finite and nonnegative",
            }
            .fail();
        }
        Ok(())
    }
}

/// One provider's config: animist's own serializable settings for the
/// kind, plus craft's model table and discovery switch. Every field the
/// provider accepts is animist's documented field — new upstream fields
/// reach users with no craft change — and `api_key` is rejected at parse
/// time (keys live in the environment, named by `api_key_env`).
#[derive(Clone, Deserialize)]
pub struct ProviderEntry<C> {
    /// The kind's animist settings (`#[serde(flatten)]`).
    #[serde(flatten)]
    pub config: C,
    /// Disable discovery for compatible servers without a models endpoint.
    #[serde(default = "default_discover_models")]
    pub discover_models: bool,
    /// Manual model table: metadata overrides keyed by exact model ID. An
    /// entry with no fields still registers an otherwise unlisted model.
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

impl<C> std::fmt::Debug for ProviderEntry<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Credential-bearing configs have no Debug by design; the kind
        // name and the craft-owned fields carry everything a log needs.
        f.debug_struct("ProviderEntry")
            .field("config", &"<per-kind settings>")
            .field("discover_models", &self.discover_models)
            .field("models", &self.models)
            .finish()
    }
}

impl<C: Default> Default for ProviderEntry<C> {
    fn default() -> Self {
        Self {
            config: C::default(),
            discover_models: default_discover_models(),
            models: BTreeMap::new(),
        }
    }
}

/// One configured provider, tagged by kind. The variants carry animist's
/// config types directly; the serde tag is the config `kind` string. The
/// manual [`Deserialize`] impl parses kind-first so dropped kinds get
/// migration guidance.
#[derive(Clone)]
pub enum ProviderConfig {
    Anthropic(ProviderEntry<animist::provider::anthropic::AnthropicConfig>),
    Openai(ProviderEntry<animist::provider::openai::OpenAiConfig>),
    /// Third-party OpenAI-compatible endpoints: the OpenAI config with the
    /// Chat Completions protocol by default. A keyless local endpoint
    /// rides animist's keyless llama.cpp client.
    OpenaiCompatible(ProviderEntry<animist::provider::openai::OpenAiConfig>),
    Synthetic(ProviderEntry<animist::provider::synthetic::SyntheticConfig>),
    ZaiCodingPlan(ProviderEntry<animist::provider::zai_coding_plan::ZaiCodingPlanConfig>),
    Ollama(ProviderEntry<animist::provider::ollama::OllamaConfig>),
    LlamaCpp(ProviderEntry<animist::provider::llama_cpp::LlamaCppConfig>),
    Omlx(ProviderEntry<animist::provider::omlx::OmlxConfig>),
    AmazonBedrock(ProviderEntry<animist::provider::bedrock::BedrockConfig>),
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Kind name plus the entry wrapper's Debug; the per-kind configs
        // have no Debug by design (credential policy).
        match self {
            Self::Anthropic(entry) => f.debug_tuple("Anthropic").field(entry).finish(),
            Self::Openai(entry) => f.debug_tuple("Openai").field(entry).finish(),
            Self::OpenaiCompatible(entry) => {
                f.debug_tuple("OpenaiCompatible").field(entry).finish()
            }
            Self::Synthetic(entry) => f.debug_tuple("Synthetic").field(entry).finish(),
            Self::ZaiCodingPlan(entry) => f.debug_tuple("ZaiCodingPlan").field(entry).finish(),
            Self::Ollama(entry) => f.debug_tuple("Ollama").field(entry).finish(),
            Self::LlamaCpp(entry) => f.debug_tuple("LlamaCpp").field(entry).finish(),
            Self::Omlx(entry) => f.debug_tuple("Omlx").field(entry).finish(),
            Self::AmazonBedrock(entry) => f.debug_tuple("AmazonBedrock").field(entry).finish(),
        }
    }
}

impl ProviderConfig {
    /// The kind string (the serde tag): registry keys and error text.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Anthropic(_) => "anthropic",
            Self::Openai(_) => "openai",
            Self::OpenaiCompatible(_) => "openai-compatible",
            Self::Synthetic(_) => "synthetic",
            Self::ZaiCodingPlan(_) => "zai-coding-plan",
            Self::Ollama(_) => "ollama",
            Self::LlamaCpp(_) => "llama-cpp",
            Self::Omlx(_) => "omlx",
            Self::AmazonBedrock(_) => "amazon-bedrock",
        }
    }

    /// The manual model table.
    pub fn models(&self) -> &BTreeMap<String, ModelConfig> {
        macro_rules! models {
            ($($variant:ident),+ $(,)?) => {
                match self {
                    $(Self::$variant(entry) => &entry.models,)+
                }
            };
        }
        models!(
            Anthropic,
            Openai,
            OpenaiCompatible,
            Synthetic,
            ZaiCodingPlan,
            Ollama,
            LlamaCpp,
            Omlx,
            AmazonBedrock,
        )
    }

    /// Whether discovery runs for this provider.
    pub fn discover_models(&self) -> bool {
        macro_rules! discover {
            ($($variant:ident),+ $(,)?) => {
                match self {
                    $(Self::$variant(entry) => entry.discover_models,)+
                }
            };
        }
        discover!(
            Anthropic,
            Openai,
            OpenaiCompatible,
            Synthetic,
            ZaiCodingPlan,
            Ollama,
            LlamaCpp,
            Omlx,
            AmazonBedrock,
        )
    }

    /// Override whether discovery runs for this provider.
    pub fn set_discover_models(&mut self, discover: bool) {
        macro_rules! set_discover {
            ($($variant:ident),+ $(,)?) => {
                match self {
                    $(Self::$variant(entry) => entry.discover_models = discover,)+
                }
            };
        }
        set_discover!(
            Anthropic,
            Openai,
            OpenaiCompatible,
            Synthetic,
            ZaiCodingPlan,
            Ollama,
            LlamaCpp,
            Omlx,
            AmazonBedrock,
        );
    }

    /// The configured base URL, when the kind has one and the user set it
    /// away from the default.
    pub fn base_url(&self) -> Option<&str> {
        fn non_default<'a>(url: &'a str, default: &str) -> Option<&'a str> {
            (url != default).then_some(url)
        }
        match self {
            Self::Anthropic(entry) => {
                non_default(&entry.config.base_url, "https://api.anthropic.com/v1")
            }
            Self::Openai(entry) | Self::OpenaiCompatible(entry) => {
                non_default(&entry.config.base_url, "https://api.openai.com/v1")
            }
            Self::Synthetic(entry) => non_default(
                &entry.config.base_url,
                animist::provider::synthetic::BASE_URL,
            ),
            Self::ZaiCodingPlan(entry) => non_default(
                &entry.config.base_url,
                animist::provider::zai_coding_plan::BASE_URL,
            ),
            Self::Ollama(entry) => non_default(&entry.config.base_url, "http://localhost:11434/v1"),
            Self::LlamaCpp(entry) => {
                non_default(&entry.config.base_url, "http://localhost:9931/v1")
            }
            Self::Omlx(entry) => non_default(&entry.config.base_url, "http://localhost:8000/v1"),
            // Bedrock endpoints are `endpoint_url`/`control_plane_endpoint_url`.
            Self::AmazonBedrock(entry) => entry
                .config
                .endpoint_url
                .as_deref()
                .or(entry.config.control_plane_endpoint_url.as_deref()),
        }
    }

    /// The environment variable holding the credential, when this kind
    /// uses one: the configured `api_key_env`, else the kind's default.
    /// Bedrock (AWS chain) and Ollama have none.
    pub fn credential_env(&self) -> Option<&str> {
        match self {
            Self::Anthropic(entry) => Some(&entry.config.api_key_env),
            Self::Openai(entry) | Self::OpenaiCompatible(entry) => Some(&entry.config.api_key_env),
            Self::Synthetic(entry) => Some(&entry.config.api_key_env),
            Self::ZaiCodingPlan(entry) => Some(&entry.config.api_key_env),
            Self::LlamaCpp(entry) => entry.config.api_key_env.as_deref(),
            Self::Omlx(entry) => entry.config.api_key_env.as_deref(),
            Self::Ollama(_) | Self::AmazonBedrock(_) => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(name) = self.credential_env() {
            if name.trim().is_empty() || name.contains(['=', '\0']) {
                return InvalidSnafu {
                    reason: "api_key_env must be a nonempty environment variable name",
                }
                .fail();
            }
        }
        for (id, model) in self.models() {
            if id.trim().is_empty() {
                return InvalidSnafu {
                    reason: "model IDs must not be empty",
                }
                .fail();
            }
            if model.context_length == Some(0) || model.max_output_tokens == Some(0) {
                return InvalidSnafu {
                    reason: format!("model {id:?}: token limits must be positive"),
                }
                .fail();
            }
            if model.protocol.is_some() && !matches!(self, Self::AmazonBedrock(_)) {
                return InvalidSnafu {
                    reason: format!("model {id:?}: protocol is only supported by amazon-bedrock"),
                }
                .fail();
            }
        }
        if let Some(base) = self.base_url() {
            if !base.is_empty() {
                let url = url::Url::parse(base).context(InvalidBaseUrlSnafu)?;
                if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                    return InvalidSnafu {
                        reason: "base_url must be HTTP(S) with a host",
                    }
                    .fail();
                }
                if !url.username().is_empty() || url.password().is_some() {
                    return InvalidSnafu {
                        reason: "base_url must not embed credentials; use api_key_env",
                    }
                    .fail();
                }
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ProviderConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;

        // Kind-first parsing: dropped kinds get migration guidance instead
        // of an unknown-variant error, and inline keys are rejected before
        // any provider sees them.
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_json(value).map_err(D::Error::custom)
    }
}

/// The supported kind strings, for parse errors.
const SUPPORTED_KINDS: &[&str] = &[
    "anthropic",
    "openai",
    "openai-compatible",
    "synthetic",
    "zai-coding-plan",
    "ollama",
    "llama-cpp",
    "omlx",
    "amazon-bedrock",
];

/// Keys owned by the craft wrapper; every other key belongs to the
/// flattened animist config.
const ENTRY_KEYS: &[&str] = &["kind", "discover_models", "models"];

impl ProviderConfig {
    fn from_json(value: serde_json::Value) -> std::result::Result<Self, String> {
        let map = value
            .as_object()
            .ok_or_else(|| "a provider block must be a table".to_owned())?;
        let kind = map
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "provider block needs kind = \"...\" (one of {})",
                    SUPPORTED_KINDS.join(", ")
                )
            })?;
        reject_inline_key(map)?;

        macro_rules! parse {
            ($variant:ident, $config:ty) => {{
                let entry: ProviderEntry<$config> = serde_json::from_value(value.clone())
                    .map_err(|error| format!("{kind} provider: {error}"))?;
                reject_unknown_keys(map, &entry.config, ENTRY_KEYS)?;
                Self::$variant(entry)
            }};
        }
        let config = match kind {
            "anthropic" => parse!(Anthropic, animist::provider::anthropic::AnthropicConfig),
            "openai" => parse!(Openai, animist::provider::openai::OpenAiConfig),
            "openai-compatible" => {
                let mut entry: ProviderEntry<animist::provider::openai::OpenAiConfig> =
                    serde_json::from_value(value.clone())
                        .map_err(|error| format!("{kind} provider: {error}"))?;
                reject_unknown_keys(map, &entry.config, ENTRY_KEYS)?;
                // Chat Completions is the compat default; an explicit
                // `protocol` in the block wins.
                if !map.contains_key("protocol") {
                    entry.config.protocol = animist::provider::openai::Protocol::ChatCompletions;
                }
                Self::OpenaiCompatible(entry)
            }
            "synthetic" => parse!(Synthetic, animist::provider::synthetic::SyntheticConfig),
            "zai-coding-plan" => parse!(
                ZaiCodingPlan,
                animist::provider::zai_coding_plan::ZaiCodingPlanConfig
            ),
            "ollama" => parse!(Ollama, animist::provider::ollama::OllamaConfig),
            "llama-cpp" => parse!(LlamaCpp, animist::provider::llama_cpp::LlamaCppConfig),
            "omlx" => parse!(Omlx, animist::provider::omlx::OmlxConfig),
            "amazon-bedrock" => parse!(AmazonBedrock, animist::provider::bedrock::BedrockConfig),
            other => return Err(dropped_kind_error(other)),
        };
        Ok(config)
    }
}

/// Keys never stored, only rejected with instructions.
fn reject_inline_key(
    map: &serde_json::Map<String, serde_json::Value>,
) -> std::result::Result<(), String> {
    if map.contains_key("api_key") {
        return Err(
            "api_key must not be stored in config files; put the key in an \
             environment variable and name it with api_key_env"
                .to_owned(),
        );
    }
    Ok(())
}

/// Report keys the provider config does not know, replacing the typo
/// protection `deny_unknown_fields` cannot provide through `flatten`:
/// a key is known when the serialized default of the kind's config has it.
fn reject_unknown_keys<C: serde::Serialize>(
    map: &serde_json::Map<String, serde_json::Value>,
    config: &C,
    entry_keys: &[&str],
) -> std::result::Result<(), String> {
    let Ok(serde_json::Value::Object(known)) = serde_json::to_value(config) else {
        return Ok(());
    };
    let unknown: Vec<&String> = map
        .keys()
        .filter(|key| !known.contains_key(*key) && !entry_keys.contains(&key.as_str()))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let mut sorted: Vec<&str> = known
        .keys()
        .map(String::as_str)
        .chain(entry_keys.iter().copied())
        .collect();
    sorted.sort_unstable();
    Err(format!(
        "unknown provider field(s) {}; known fields are {}",
        unknown
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        sorted.join(", ")
    ))
}

/// The actionable error for a config naming a dropped kind: OpenAI-dialect
/// vendors learn the `openai-compatible` migration with their base URL.
fn dropped_kind_error(kind: &str) -> String {
    let openai_dialect = [
        ("deepseek", "https://api.deepseek.com"),
        ("doubleword", ""),
        ("groq", "https://api.groq.com/openai/v1"),
        ("huggingface", "https://router.huggingface.co/v1"),
        ("hyperbolic", "https://api.hyperbolic.xyz/v1"),
        ("minimax", "https://api.minimax.chat/v1"),
        ("mira", ""),
        ("mistral", "https://api.mistral.ai/v1"),
        ("moonshot", "https://api.moonshot.ai/v1"),
        ("openrouter", "https://openrouter.ai/api/v1"),
        ("perplexity", "https://api.perplexity.ai"),
        ("together", "https://api.together.xyz/v1"),
        ("venice", "https://api.venice.ai/api/v1"),
        ("xai", "https://api.x.ai/v1"),
        ("xiaomimimo", ""),
        ("zai", "https://api.z.ai/api/paas/v4"),
    ];
    if let Some((_, base)) = openai_dialect.iter().find(|(name, _)| *name == kind) {
        return if base.is_empty() {
            format!(
                "provider kind {kind:?} is no longer built in; point \
                 kind = \"openai-compatible\" at the vendor's OpenAI-compatible base_url"
            )
        } else {
            format!(
                "provider kind {kind:?} is no longer built in; use \
                 kind = \"openai-compatible\" with base_url = \"{base}\""
            )
        };
    }
    match kind {
        "llamafile" => "provider kind \"llamafile\" is no longer built in; use \
                        kind = \"llama-cpp\" with base_url pointing at the server"
            .to_owned(),
        "azure" => "provider kind \"azure\" is no longer built in; use \
                    kind = \"openai-compatible\" with the deployment's base_url"
            .to_owned(),
        "gemini" => "provider kind \"gemini\" is no longer built in; use \
                     kind = \"openai-compatible\" with \
                     base_url = \"https://generativelanguage.googleapis.com/v1beta/openai\""
            .to_owned(),
        "cohere" => "provider kind \"cohere\" is no longer built in; use kind = \
             \"openai-compatible\" with base_url = \"https://api.cohere.com/compatibility/v1\""
            .to_owned(),
        "chatgpt" | "copilot" => format!(
            "provider kind {kind:?} is no longer built in: the OAuth device flows were \
             removed. Use API-key access through the vendor's API kind"
        ),
        "voyageai" => "provider kind \"voyageai\" is no longer built in (embeddings \
                       only; no completion models)"
            .to_owned(),
        other => format!(
            "unknown provider kind {other:?}; supported kinds are {}",
            SUPPORTED_KINDS.join(", ")
        ),
    }
}

fn default_discover_models() -> bool {
    true
}

/// Partial metadata overrides, keyed by the exact provider model/deployment
/// ID. An empty table is sufficient to register an otherwise unlisted model.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub name: Option<String>,
    pub description: Option<String>,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub supports_thinking: Option<bool>,
    pub requires_thinking: Option<bool>,
    /// Bedrock only: the wire protocol for this model, overriding the
    /// per-family routing animist never infers.
    pub protocol: Option<animist::provider::bedrock::Protocol>,
}

impl Config {
    pub async fn load() -> Result<Self> {
        let loaded = crate::bml::load_global();
        if let Some((path, error)) = loaded.errors.first() {
            return Err(error.clone())
                .context(InvalidBmlSnafu)
                .context(LoadConfigSnafu { path: path.clone() });
        }
        match loaded.doc {
            Some(doc) => Self::from_statement(&doc),
            None => {
                crate::bml::warn_legacy_toml(false);
                Ok(Self::default())
            }
        }
    }

    /// A missing file means no configured providers. Other I/O errors are fatal.
    pub async fn load_from(path: &Path) -> Result<Self> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).context(ReadConfigSnafu {
                    path: path.to_path_buf(),
                });
            }
        };
        Self::parse(&text).with_context(|_| LoadConfigSnafu {
            path: path.to_path_buf(),
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        // An empty document keeps every default; barkml itself rejects it.
        if text.trim().is_empty() {
            let doc = barkml::Statement::new_module(
                "main",
                indexmap::IndexMap::new(),
                barkml::Metadata::default(),
            );
            return Self::from_statement(&doc);
        }
        let doc = crate::bml::parse(text).context(InvalidBmlSnafu)?;
        let config = Self::from_statement(&doc)?;
        Self::validate(config)
    }

    /// Extract a [`Config`] from a merged BarkML document, taking only the
    /// sections that belong to the main config (`agent`, `provider`,
    /// `compaction`, `compression`, `keybind`/`keybindings`); unrelated
    /// blocks (`mcp`, `permissions`) are left for their own loaders.
    pub fn from_statement(doc: &barkml::Statement) -> Result<Self> {
        use serde_json::Value as Json;
        let mut map = serde_json::Map::new();

        let root = crate::bml::container_json(doc);
        if let Some(value) = root.get("always_thinking") {
            map.insert("always_thinking".into(), value.clone());
        }
        for section in ["agent", "compression"] {
            if let Some(child) = doc.get_child(section, &[]) {
                map.insert(section.to_string(), crate::bml::container_json(child));
            }
        }

        // `provider "name" { ... }` labeled blocks → providers map.
        let mut providers = serde_json::Map::new();
        for (id, labels, block) in doc.blocks() {
            if id != "provider" {
                continue;
            }
            let Some(name) = labels.first().and_then(|l| l.as_string().cloned()) else {
                continue;
            };
            let mut json = crate::bml::container_json(block);
            // Labeled `model "id" { ... }` children collect under `model`;
            // the struct field is `models`.
            if let Some(models) = json.as_object_mut().unwrap().remove("model") {
                json.as_object_mut()
                    .unwrap()
                    .insert("models".to_string(), models);
            }
            providers.insert(name, json);
        }
        if !providers.is_empty() {
            map.insert("providers".to_string(), Json::Object(providers));
        }

        // `compaction "kind" { ... }` labeled blocks → ordered stages.
        let mut stages = Vec::new();
        for (id, labels, block) in doc.blocks() {
            if id != "compaction" {
                continue;
            }
            let mut json = crate::bml::container_json(block);
            if let Some(label) = labels.first().and_then(|l| l.as_string().cloned())
                && !json.as_object().is_some_and(|o| o.contains_key("kind"))
            {
                json.as_object_mut()
                    .unwrap()
                    .insert("kind".to_string(), Json::String(label));
            }
            stages.push(json);
        }
        if !stages.is_empty() {
            map.insert("compaction".to_string(), Json::Array(stages));
        }

        if let Some(value) = doc
            .get_child("compaction_buffer", &[])
            .and_then(|child| child.get_value())
        {
            map.insert(
                "compaction_buffer".to_string(),
                crate::bml::value_json(value),
            );
        }

        // `keybind "action" { keys = [...] }` or `keybindings = { ... }`.
        let mut keybindings = serde_json::Map::new();
        for (id, labels, block) in doc.blocks() {
            if id != "keybind" {
                continue;
            }
            let Some(action) = labels.first().and_then(|l| l.as_string().cloned()) else {
                continue;
            };
            let keys = block
                .get_child("keys", &[])
                .and_then(|k| k.get_value())
                .map(crate::bml::value_json)
                .unwrap_or(Json::Array(Vec::new()));
            keybindings.insert(action, keys);
        }
        if !keybindings.is_empty() {
            map.insert("keybindings".to_string(), Json::Object(keybindings));
        } else if let Some(value) = doc
            .get_child("keybindings", &[])
            .and_then(|child| child.get_value())
        {
            map.insert("keybindings".to_string(), crate::bml::value_json(value));
        }

        let config: Self = serde_json::from_value(Json::Object(map)).map_err(|e| {
            InvalidSnafu {
                reason: e.to_string(),
            }
            .build()
        })?;
        Self::validate(config)
    }

    fn validate(config: Self) -> Result<Self> {
        config.agent.validate()?;
        if let Err(reason) = config.compression.validate() {
            return InvalidSnafu { reason }.fail();
        }
        let mut seen_kinds = std::collections::BTreeSet::new();
        for stage in &config.compaction {
            if !stage.context.is_finite() || stage.context <= 0.0 || stage.context >= 1.0 {
                return InvalidSnafu {
                    reason: format!(
                        "compaction stage {:?}: context must be between 0 and 1",
                        stage.kind
                    ),
                }
                .fail();
            }
            if !seen_kinds.insert(stage.kind) {
                return InvalidSnafu {
                    reason: format!(
                        "compaction stage {:?}: each kind may appear at most once",
                        stage.kind
                    ),
                }
                .fail();
            }
        }
        for (name, provider) in &config.providers {
            if name.trim().is_empty() {
                return InvalidSnafu {
                    reason: "provider names must not be empty",
                }
                .fail();
            }
            provider
                .validate()
                .with_context(|_| InvalidProviderSnafu { name: name.clone() })?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_defaults_and_model_overrides_parse() {
        use crate::thinking::{Effort, ThinkingConfig};
        let config = Config::parse(
            r#"
            always_thinking = "low"
            agent { thinking = 4096 }
            provider "local" {
              kind = "openai-compatible"
              model "custom" {
                supports_thinking = true
                requires_thinking = true
              }
            }
        "#,
        )
        .unwrap();
        assert_eq!(
            config.always_thinking,
            Some(ThinkingConfig::Effort(Effort::Low))
        );
        assert_eq!(config.agent.thinking, Some(ThinkingConfig::Budget(4096)));
        let model = &config.providers["local"].models()["custom"];
        assert_eq!(model.supports_thinking, Some(true));
        assert_eq!(model.requires_thinking, Some(true));
        for value in ["0", "\"nonsense\""] {
            assert!(Config::parse(&format!("always_thinking = {value}")).is_err());
        }
    }

    #[test]
    fn parses_example() {
        let config = Config::parse(include_str!("../agent.example.bml")).unwrap();
        assert_eq!(config.providers.len(), 4);
        assert_eq!(config.agent.preamble, "");
        assert_eq!(
            config.compression,
            crate::compression::CompressionConfig::default()
        );
        assert_eq!(
            config.compaction,
            vec![
                CompactionConfig {
                    kind: CompactionKind::Vcc,
                    context: 0.6,
                },
                CompactionConfig {
                    kind: CompactionKind::Llm,
                    context: 0.8,
                },
            ]
        );
    }

    #[test]
    fn prompt_format_parses_both_values_and_rejects_unknown() {
        use crate::prompt::PromptFormat;
        let xml = Config::parse("agent { prompt_format = \"xml\" }").unwrap();
        assert_eq!(xml.agent.prompt_format, PromptFormat::Xml);
        let markdown = Config::parse("agent { prompt_format = \"markdown\" }").unwrap();
        assert_eq!(markdown.agent.prompt_format, PromptFormat::Markdown);
        assert!(
            Config::parse("agent { prompt_format = \"yaml\" }").is_err(),
            "an unknown format is rejected, not defaulted"
        );
        let default = Config::parse("").unwrap();
        assert_eq!(default.agent.prompt_format, PromptFormat::Markdown);
    }

    #[test]
    fn compaction_defaults_to_vcc_then_llm() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.compaction.len(), 2);
        assert_eq!(config.compaction[0].kind, CompactionKind::Vcc);
        assert!((config.compaction[0].context - 0.6).abs() < 1e-9);
        assert_eq!(config.compaction[1].kind, CompactionKind::Llm);
        assert!((config.compaction[1].context - 0.8).abs() < 1e-9);
        assert_eq!(config.compaction_buffer, DEFAULT_COMPACTION_BUFFER);
    }

    #[test]
    fn compaction_buffer_parses_tokens_and_percent() {
        let tokens = Config::parse("compaction_buffer = 20000").unwrap();
        assert_eq!(tokens.compaction_buffer, CompactionBuffer::Tokens(20000));
        let percent = Config::parse("compaction_buffer = \"20%\"").unwrap();
        assert_eq!(percent.compaction_buffer, CompactionBuffer::Percent(20));
        assert!(Config::parse("compaction_buffer = 100").is_err());
        assert!(Config::parse("compaction_buffer = \"20\"").is_err());
    }

    #[test]
    fn compaction_buffer_resolves_against_window() {
        assert_eq!(CompactionBuffer::Tokens(20000).resolve(100_000), 20_000);
        assert_eq!(CompactionBuffer::Percent(20).resolve(100_000), 20_000);
        assert_eq!(CompactionBuffer::Percent(20).resolve(0), 0);
        assert_eq!(CompactionBuffer::Percent(33).resolve(1000), 330);
    }

    #[test]
    fn compression_defaults_when_absent() {
        let config = Config::parse("").unwrap();
        assert_eq!(
            config.compression,
            crate::compression::CompressionConfig::default()
        );
    }

    #[test]
    fn compression_accepts_overrides() {
        let config = Config::parse(
            "compression {\n  enabled = false\n  code_compression_rate = 0.5\n  max_log_lines = 25\n}",
        )
        .unwrap();
        assert!(!config.compression.enabled);
        assert!((config.compression.code_compression_rate - 0.5).abs() < 1e-6);
        assert_eq!(config.compression.max_log_lines, 25);
        assert_eq!(config.compression.max_diff_lines, 100);
    }

    #[test]
    fn rejects_invalid_compression_rate() {
        assert!(Config::parse("compression { code_compression_rate = 0.0 }").is_err());
        assert!(Config::parse("compression { code_compression_rate = 1.5 }").is_err());
        assert!(Config::parse("compression { no_such_knob = 1 }").is_err());
    }

    #[test]
    fn compaction_accepts_string_or_number_ratios() {
        let config = Config::parse(
            "compaction \"llm\" { context = \"0.75\" }\ncompaction \"vcc\" { context = 0.5 }",
        )
        .unwrap();
        assert_eq!(config.compaction.len(), 2);
        assert!((config.compaction[0].context - 0.75).abs() < 1e-9);
        assert!((config.compaction[1].context - 0.5).abs() < 1e-9);
    }

    #[test]
    fn rejects_invalid_compaction_settings() {
        for text in [
            "compaction \"unknown\" { context = 0.5 }",
            "compaction \"llm\" { context = 0 }",
            "compaction \"llm\" { context = 1.0 }",
            "compaction \"llm\" { context = \"not a number\" }",
            "compaction \"llm\" { }",
            "compaction \"llm\" { context = 0.5\n  extra = true }",
            "compaction \"vcc\" { context = 0.5 }\ncompaction \"vcc\" { context = 0.7 }",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn agent_config_is_optional_and_supports_partial_overrides() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.agent.temperature, None);
        assert_eq!(config.agent.max_tokens, None);
        let config = Config::parse("agent { max_tokens = 1024 }").unwrap();
        assert_eq!(config.agent.max_tokens, Some(1024));
        let config = Config::parse("agent { max_turns = 4 }").unwrap();
        assert_eq!(config.agent.max_turns, Some(4));
        assert_eq!(config.agent.preamble, AgentConfig::default().preamble);
    }

    #[test]
    fn rejects_invalid_agent_settings() {
        for field in [
            "max_turns = 0",
            "max_tokens = 0",
            "temperature = -0.1",
            "temperature = nan",
            "temperature = inf",
            "unknown_setting = true",
        ] {
            assert!(
                Config::parse(&format!("agent {{ {field} }}")).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn rejects_invalid_configuration() {
        for text in [
            "provider \"x\" { kind = \"unknown\" }",
            "provider \"x\" { kind = \"openai\"\n  api_key = \"do-not-store-keys\" }",
            "provider \"x\" { kind = \"openai\"\n  base_url = \"file:///tmp/api\" }",
            "provider \"x\" { kind = \"openai\"\n  base_url = \"https://user:secret@example.com\" }",
            "provider \"x\" { kind = \"openai\"\n  api_version = \"v1\" }",
            "provider \"x\" { kind = \"openai\"\n  api_key_env = \"\" }",
            "provider \"x\" { kind = \"openai\"\n  model \"test\" { context_length = 0 } }",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn split_layout_merges_deep_and_labeled() {
        let dir = tempfile::tempdir().unwrap();
        let xdg = dir.path().join("craft");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(
            dir.path().join("craft.bml"),
            "agent { temperature = 0.2 }\nprovider \"x\" { kind = \"openai\" }\ncompaction \"vcc\" { context = 0.5 }",
        )
        .unwrap();
        std::fs::write(
            xdg.join("extra.bml"),
            "agent { max_tokens = 512 }\nprovider \"x\" { base_url = \"https://llm.example.com\" }\ncompaction \"llm\" { context = 0.9 }",
        )
        .unwrap();

        let loaded = crate::bml::load_global_from(std::slice::from_ref(&xdg), Some(xdg.as_path()));
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let doc = loaded.doc.expect("bml sources exist");
        let config = Config::from_statement(&doc).unwrap();
        // Deep merge: both agent fields apply.
        assert_eq!(config.agent.temperature, Some(0.2));
        assert_eq!(config.agent.max_tokens, Some(512));
        // Repeated blocks accumulate.
        assert_eq!(config.compaction.len(), 2);
        // Labeled block override merges per-field.
        let provider = &config.providers["x"];
        assert_eq!(provider.base_url(), Some("https://llm.example.com"));
    }

    #[test]
    fn legacy_toml_without_bml_is_detected_and_bml_wins() {
        let dir = tempfile::tempdir().unwrap();
        // No bml anywhere: a legacy agent.toml must be flagged.
        std::fs::write(dir.path().join("agent.toml"), "[agent]\n").unwrap();
        let loaded = crate::bml::load_global_from(&[dir.path().to_path_buf()], None);
        assert!(loaded.doc.is_none());
        assert!(!crate::bml::legacy_toml_files_in(&[dir.path().to_path_buf()]).is_empty());

        // With bml present, the legacy file is not flagged as blocking.
        std::fs::write(dir.path().join("craft.bml"), "agent { max_tokens = 1 }").unwrap();
        let loaded = crate::bml::load_global_from(&[dir.path().to_path_buf()], None);
        let doc = loaded.doc.expect("bml source found");
        let config = Config::from_statement(&doc).unwrap();
        assert_eq!(config.agent.max_tokens, Some(1));
        assert_eq!(config.providers.len(), 0);
    }

    #[tokio::test]
    async fn missing_config_is_empty() {
        let path = std::env::temp_dir().join(format!(
            "craft-agent-missing-{}-{}.bml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        assert!(Config::load_from(&path).await.unwrap().providers.is_empty());
    }
}

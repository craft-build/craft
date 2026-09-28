//! G.6 setup & first-run flow, ported from the reference `src/setup.rs`.
//!
//! The reference resolves a working provider at startup without any config:
//! `PROVIDER_PRIORITY` walks well-known providers, `provider_available`
//! checks their credential, and the failure path is an actionable error
//! ("set an API key … or use -m"). Despite the comparison entry's
//! "interactive" wording, the reference has no wizard — trust the code.
//!
//! Adapted to this repo's config-driven Rig architecture: when
//! `agent.toml` configures no providers, auto-detect providers from
//! credential environment variables and inject them as in-memory config
//! entries (priority order preserved as the config key order, which the
//! default model selection then walks). Explicit configuration always
//! wins; auto-detection only fills an empty provider map.

use crate::config::{Config, ProviderConfig};
use crate::providers::ProviderKind;

/// Credential-bearing kinds in preference order, adapted from the
/// reference's `PROVIDER_PRIORITY` (anthropic, openai, xai, …) to the
/// kinds whose default key env var this repo's registry table names.
/// Local/factory-auth kinds (ollama, llamafile, azure, chatgpt, copilot)
/// never carry a default key env var and are skipped.
const PROVIDER_PRIORITY: &[ProviderKind] = &[
    ProviderKind::Anthropic,
    ProviderKind::Openai,
    ProviderKind::Xai,
    ProviderKind::Deepseek,
    ProviderKind::Gemini,
    ProviderKind::Groq,
    ProviderKind::Mistral,
    ProviderKind::Openrouter,
    ProviderKind::Together,
    ProviderKind::Moonshot,
    ProviderKind::Zai,
    ProviderKind::Perplexity,
    ProviderKind::Cohere,
    ProviderKind::Hyperbolic,
    ProviderKind::Venice,
    ProviderKind::Minimax,
    ProviderKind::Mira,
    ProviderKind::Xiaomimimo,
];

/// One auto-detected provider: config name equals the kind slug.
pub struct DetectedProvider {
    pub name: String,
    pub env_var: &'static str,
    pub config: ProviderConfig,
}

/// Providers whose default credential env var is set to a nonempty value,
/// in [`PROVIDER_PRIORITY`] order. Injectable env lookup keeps process-wide
/// environment access out of tests.
pub fn detect_providers(env: &dyn Fn(&str) -> Option<String>) -> Vec<DetectedProvider> {
    PROVIDER_PRIORITY
        .iter()
        .filter_map(|&kind| {
            let env_var = kind.api_key_env_default()?;
            let available = env(env_var).is_some_and(|v| !v.trim().is_empty());
            available.then(|| DetectedProvider {
                name: kind.as_str().to_owned(),
                env_var,
                config: ProviderConfig {
                    kind,
                    api_key_env: None,
                    base_url: None,
                    api_version: None,
                    account_id: None,
                    discover_models: true,
                    models: Default::default(),
                },
            })
        })
        .collect()
}

/// First-run fill: when no providers are configured, inject the
/// auto-detected ones. Returns user-facing notes (one per detected
/// provider); an empty provider map stays empty.
pub fn first_run(config: &mut Config) -> Vec<String> {
    if !config.providers.is_empty() {
        return Vec::new();
    }
    let detected = detect_providers(&|name| std::env::var(name).ok());
    let notes = detected
        .iter()
        .map(|d| format!("first run: using provider {} from ${}", d.name, d.env_var))
        .collect();
    config
        .providers
        .extend(detected.into_iter().map(|d| (d.name, d.config)));
    notes
}

/// The actionable message when no provider is available, matching the
/// reference `resolve_model` error: name the fix, not just the failure.
pub fn setup_hint() -> String {
    "no provider available - set an API key (e.g. ANTHROPIC_API_KEY), \
     configure ~/.config/craft/agent.toml, or run `craft doctor`"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            map.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    fn detected_names(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        detect_providers(env).into_iter().map(|d| d.name).collect()
    }

    #[test]
    fn detection_follows_priority_order_not_env_order() {
        // xai's key is listed first, but anthropic outranks it.
        let env = env_of(&[("XAI_API_KEY", "k"), ("ANTHROPIC_API_KEY", "k")]);
        assert_eq!(
            detected_names(&env),
            vec!["anthropic".to_owned(), "xai".to_owned()]
        );
    }

    #[test]
    fn blank_credentials_are_ignored() {
        let env = env_of(&[("ANTHROPIC_API_KEY", "  ")]);
        assert!(detected_names(&env).is_empty());
    }

    #[test]
    fn nothing_set_detects_nothing() {
        assert!(detected_names(&env_of(&[])).is_empty());
    }

    #[test]
    fn detected_config_uses_default_credential_env() {
        let env = env_of(&[("OPENAI_API_KEY", "sk-x")]);
        let detected = detect_providers(&env).pop().expect("one provider");
        assert_eq!(detected.config.kind, ProviderKind::Openai);
        // api_key_env stays None so the registry's default env var applies.
        assert!(detected.config.api_key_env.is_none());
        assert!(detected.config.validate().is_ok());
    }

    #[test]
    fn first_run_only_fills_an_empty_provider_map() {
        let mut config = Config::default();
        config.providers.insert(
            "mine".to_owned(),
            ProviderConfig {
                kind: ProviderKind::Anthropic,
                api_key_env: None,
                base_url: None,
                api_version: None,
                account_id: None,
                discover_models: true,
                models: Default::default(),
            },
        );
        // ANTHROPIC_API_KEY is set in the test environment on this machine;
        // explicit config must still win.
        let notes = first_run(&mut config);
        assert!(notes.is_empty());
        assert_eq!(config.providers.len(), 1);
        assert!(config.providers.contains_key("mine"));
    }

    #[test]
    fn setup_hint_names_the_fixes() {
        let hint = setup_hint();
        assert!(hint.contains("ANTHROPIC_API_KEY"), "{hint}");
        assert!(hint.contains("agent.toml"), "{hint}");
        assert!(hint.contains("craft doctor"), "{hint}");
    }
}

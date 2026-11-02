//! Setup & first-run flow: when `craft.bml` configures no providers,
//! auto-detect providers from credential environment variables and inject
//! them as in-memory config entries (priority order preserved as the
//! config key order, which the default model selection then walks).
//! Explicit configuration always wins; auto-detection only fills an empty
//! provider map.

use crate::config::{Config, ProviderConfig, ProviderEntry};

/// Credential-bearing kinds in preference order, limited to the kinds whose
/// default key env var the animist configs name. Local kinds (ollama,
/// llama-cpp, omlx) carry no default key env var and are skipped.
const PROVIDER_PRIORITY: &[&str] = &["anthropic", "openai", "synthetic", "zai-coding-plan"];

/// The default credential env var for a supported kind, when it has one.
fn default_env(kind: &str) -> Option<&'static str> {
    match kind {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "synthetic" => Some("SYNTHETIC_API_KEY"),
        "zai-coding-plan" => Some("ZHIPU_API_KEY"),
        _ => None,
    }
}

/// A config with only its kind set, every provider field at its default.
fn default_config(kind: &str) -> Option<ProviderConfig> {
    match kind {
        "anthropic" => Some(ProviderConfig::Anthropic(ProviderEntry::default())),
        "openai" => Some(ProviderConfig::Openai(ProviderEntry::default())),
        "synthetic" => Some(ProviderConfig::Synthetic(ProviderEntry::default())),
        "zai-coding-plan" => Some(ProviderConfig::ZaiCodingPlan(ProviderEntry::default())),
        _ => None,
    }
}

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
            let env_var = default_env(kind)?;
            let config = default_config(kind)?;
            let available = env(env_var).is_some_and(|v| !v.trim().is_empty());
            available.then(|| DetectedProvider {
                name: kind.to_owned(),
                env_var,
                config,
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
     configure ~/.config/craft.bml, or run `craft doctor`"
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
        // The z.ai key is listed first, but anthropic outranks it.
        let env = env_of(&[("ZHIPU_API_KEY", "k"), ("ANTHROPIC_API_KEY", "k")]);
        assert_eq!(
            detected_names(&env),
            vec!["anthropic".to_owned(), "zai-coding-plan".to_owned()]
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
        assert_eq!(
            crate::provider::config_kind_str(&detected.config),
            "openai",
            "api_key_env stays at the provider default"
        );
        assert!(detected.config.validate().is_ok());
    }

    #[test]
    fn first_run_only_fills_an_empty_provider_map() {
        let mut config = Config::default();
        config.providers.insert(
            "mine".to_owned(),
            ProviderConfig::Anthropic(ProviderEntry::default()),
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
        assert!(hint.contains("craft.bml"), "{hint}");
        assert!(hint.contains("craft doctor"), "{hint}");
    }
}

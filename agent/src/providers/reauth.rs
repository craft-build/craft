//! Reauth: wait for refreshed credentials and rebuild the provider.

use std::sync::Arc;
use std::time::Duration;

use crate::config::ProviderConfig;

use super::Provider;

/// How often the E.10 reauth hook polls the credential, and how long it
/// waits overall before giving up (the run's cancellation still ends it
/// immediately).
const REAUTH_POLL: Duration = Duration::from_secs(2);
const REAUTH_MAX_WAIT: Duration = Duration::from_secs(600);

/// Production E.10 hook (see [`crate::run::RunParams::reauth`]): waits for
/// the provider credential to be refreshed, then rebuilds the provider and
/// returns the model for the run to retry with. A process's environment
/// only changes in-process, so this serves flows that update credentials
/// while craft runs (plugins, config reloads, the future H.5 auth flow).
pub fn reauth_hook(config: &ProviderConfig, model: &str) -> crate::run::ReauthHook {
    // Clone before the closure so no borrowed data is captured.
    let config = config.clone();
    let model = model.to_owned();
    Arc::new(move |_attempt| {
        let config = config.clone();
        let model = model.clone();
        Box::pin(async move {
            let Some(env_name) = config
                .api_key_env
                .clone()
                .or_else(|| config.kind.api_key_env_default().map(str::to_owned))
            else {
                return Err(format!(
                    "provider {} has no refreshable credential; restart after re-authenticating",
                    config.kind.as_str()
                ));
            };
            let original = std::env::var(&env_name).unwrap_or_default();
            let deadline = std::time::Instant::now() + REAUTH_MAX_WAIT;
            loop {
                let current = std::env::var(&env_name).unwrap_or_default();
                if !current.trim().is_empty() && current != original {
                    let provider = Provider::from_config(&config).map_err(|e| e.to_string())?;
                    return provider
                        .completion_model(&model)
                        .map(Some)
                        .map_err(|e| e.to_string());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "credentials in {env_name} were not refreshed within {}s; \
                         set a new value and retry",
                        REAUTH_MAX_WAIT.as_secs()
                    ));
                }
                tokio::time::sleep(REAUTH_POLL).await;
            }
        })
    })
}

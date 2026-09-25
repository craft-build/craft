//! OpenAI-compatible model discovery via `GET {base_url}/models`.

use std::sync::LazyLock;
use std::time::Duration;

use rig_core::model::{Model, ModelList};

use crate::config::ProviderConfig;
use crate::error::{InvalidSnafu, Result};

/// Shared discovery HTTP client with a bounded timeout, so one hung
/// model-listing endpoint cannot block startup indefinitely.
static DISCOVERY_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default()
});

/// `GET {base_url}/models` for OpenAI-compatible servers, keeping the optional
/// metadata (`context_length`, `description`, output limits) that Rig's shared
/// listing DTO drops. Third-party servers (synthetic, vLLM, LM Studio, ...)
/// frequently publish these; without `context_length` the ACP layer cannot
/// report context usage or trigger compaction. Used only when `base_url` is
/// configured; otherwise Rig's lister covers the default OpenAI endpoint.
pub(crate) async fn list_openai_compatible_models(
    config: &ProviderConfig,
    credential: &(dyn Fn(&str) -> Result<String> + Send + Sync),
) -> Result<ModelList> {
    let key = credential(config.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY"))?;
    let Some(base_url) = config.base_url.as_deref() else {
        return InvalidSnafu {
            reason: "openai-compatible discovery requires a configured base_url",
        }
        .fail();
    };
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    let response = DISCOVERY_CLIENT
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(crate::error::client_error)?;
    let status = response.status();
    let body = response.text().await.map_err(crate::error::client_error)?;
    if !status.is_success() {
        return InvalidSnafu {
            reason: format!("GET {url} returned {status}: {body}"),
        }
        .fail();
    }
    let envelope: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| crate::error::Error::Invalid {
            reason: format!("GET {url} returned a malformed models listing"),
        })?;
    let models = envelope
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| crate::error::Error::Invalid {
            reason: format!("GET {url} returned no models data"),
        })?
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let mut model = Model::from_id(id);
            model.name = string_field(entry, "name");
            model.description = string_field(entry, "description");
            model.created_at =
                number_field(entry, "created").or_else(|| number_field(entry, "created_at"));
            model.owned_by = string_field(entry, "owned_by");
            model.context_length = number_field(entry, "context_length")
                .or_else(|| number_field(entry, "context_window"))
                .map(|value| value.min(u32::MAX as u64) as u32);
            model.max_output_tokens = number_field(entry, "max_output_tokens")
                .or_else(|| number_field(entry, "max_output_length"))
                .map(|value| value.min(u32::MAX as u64) as u32);
            Some(model)
        })
        .collect();
    Ok(ModelList::new(models))
}

fn string_field(entry: &serde_json::Value, field: &str) -> Option<String> {
    entry.get(field)?.as_str().map(str::to_owned)
}

fn number_field(entry: &serde_json::Value, field: &str) -> Option<u64> {
    entry.get(field)?.as_u64()
}

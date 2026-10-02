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
            record_reasoning(base, id, entry, model.max_output_tokens);
            Some(model)
        })
        .collect();
    Ok(ModelList::new(models))
}

fn record_reasoning(base: &str, id: &str, entry: &serde_json::Value, max_output: Option<u32>) {
    let Some(efforts) = entry
        .pointer("/reasoning_parameters/efforts")
        .and_then(|v| v.as_array())
    else {
        return;
    };
    let Some(values) = efforts
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    crate::thinking::record_discovered(
        base,
        id,
        crate::thinking::ModelThinking {
            supports: Some(true),
            options: vec![crate::thinking::ReasoningOption {
                kind: "effort".into(),
                values,
                ..Default::default()
            }],
            max_output,
            ..Default::default()
        },
    );
}

fn string_field(entry: &serde_json::Value, field: &str) -> Option<String> {
    entry.get(field)?.as_str().map(str::to_owned)
}

fn number_field(entry: &serde_json::Value, field: &str) -> Option<u64> {
    entry.get(field)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thinking::{Effort, ThinkingConfig};

    #[test]
    fn discovered_efforts_filter_aliases_and_drive_requests() {
        let config = crate::config::Config::parse(
            r#"
            provider "gateway" {
                kind = "openai-compatible"
                base_url = "https://discovery-efforts.test/v1/"
            }
            "#,
        )
        .unwrap();
        let provider = &config.providers["gateway"];
        for (id, efforts, levels) in [
            (
                "syn:large:text",
                vec!["none", "low", "high", "xhigh", "max"],
                vec![Effort::Low, Effort::High, Effort::XHigh, Effort::Max],
            ),
            (
                "syn:large:vision",
                vec!["low", "high", "max"],
                vec![Effort::Low, Effort::High, Effort::Max],
            ),
            (
                "hf:zai-org/GLM-4.7-Flash",
                vec!["none", "low", "medium", "high"],
                vec![Effort::Low, Effort::Medium, Effort::High],
            ),
        ] {
            record_reasoning(
                "https://discovery-efforts.test/v1",
                id,
                &serde_json::json!({"reasoning_parameters": {"efforts": efforts}}),
                Some(32_768),
            );
            let choices = crate::thinking::choices_for(ThinkingConfig::Off, provider, id);
            let mut expected = vec![];
            if efforts.contains(&"none") {
                expected.push(ThinkingConfig::Off);
            }
            expected.push(ThinkingConfig::Adaptive);
            expected.extend(levels.into_iter().map(ThinkingConfig::Effort));
            assert_eq!(choices, expected);
            let info = crate::thinking::model_info_for(provider, id);
            assert_eq!(info.max_output, Some(32_768));
            let body = crate::thinking::wire(
                ThinkingConfig::Effort(Effort::Medium),
                provider.kind,
                id,
                &info,
                None,
            );
            assert_eq!(
                body["reasoning_effort"],
                if id.contains("Flash") {
                    "medium"
                } else {
                    "low"
                }
            );
        }
        let mut other_endpoint = provider.clone();
        other_endpoint.base_url = Some("https://another-gateway.test/v1".into());
        assert!(
            crate::thinking::model_info_for(&other_endpoint, "syn:large:text")
                .options
                .is_empty()
        );
        let mut override_config = provider.clone();
        override_config.models.insert(
            "syn:large:text".into(),
            crate::config::ModelConfig {
                supports_thinking: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(
            crate::thinking::choices_for(ThinkingConfig::Off, &override_config, "syn:large:text"),
            vec![ThinkingConfig::Off]
        );
    }
}

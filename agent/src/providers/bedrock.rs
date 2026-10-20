//! Bedrock model discovery through the AWS control-plane API: the inference
//! profiles (system-defined cross-region and application) the account can
//! call. The data-plane client rig-bedrock wraps has no listing operation.

use aws_sdk_bedrock::Client;
use aws_sdk_bedrock::types::InferenceProfileStatus;
use rig_core::model::{ModelInfo, ModelList};

use crate::config::ProviderConfig;
use crate::error::Result;

/// List the ACTIVE inference profiles as rig models, enriched with
/// context/output limits from the models.dev catalog. Both listing types
/// are included: system profiles (the `us.`/`eu.` cross-region spellings)
/// and application profiles the account defined.
pub(crate) async fn list_models(config: &ProviderConfig) -> Result<ModelList> {
    let client = client(config).await;
    let mut models = Vec::new();
    for profile_type in [
        aws_sdk_bedrock::types::InferenceProfileType::SystemDefined,
        aws_sdk_bedrock::types::InferenceProfileType::Application,
    ] {
        let mut page = client
            .list_inference_profiles()
            .type_equals(profile_type)
            .into_paginator()
            .send();
        while let Some(result) = page.next().await {
            let result = result.map_err(|e| crate::error::Error::Invalid {
                reason: format!("bedrock discovery failed: {e}"),
            })?;
            for summary in result.inference_profile_summaries() {
                if *summary.status() != InferenceProfileStatus::Active {
                    continue;
                }
                let Some(model) = profile_to_model(summary) else {
                    continue;
                };
                models.push(model);
            }
        }
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(ModelList::new(models))
}

/// One ACTIVE profile summary as a catalog model, with the profile's
/// underlying base model (from its first `model_arn`) consulted when the
/// catalog has no entry for the profile id itself: a cross-region profile
/// (`us.anthropic.claude-…`) and its base model share limits.
fn profile_to_model(
    summary: &aws_sdk_bedrock::types::InferenceProfileSummary,
) -> Option<ModelInfo> {
    let id = summary.inference_profile_id();
    if id.trim().is_empty() {
        return None;
    }
    let (mut context_length, mut max_output_tokens) = limits_for(id);
    if context_length.is_none()
        && max_output_tokens.is_none()
        && let Some(base) = underlying_model(summary)
    {
        let (context, output) = limits_for(&base);
        context_length = context;
        max_output_tokens = output;
    }
    Some(ModelInfo {
        id: id.to_owned(),
        name: Some(summary.inference_profile_name().to_owned()),
        description: summary.description().map(str::to_owned),
        r#type: None,
        created_at: None,
        owned_by: Some("aws".to_owned()),
        context_length,
        max_output_tokens,
    })
}

/// Context/output limits for a Bedrock model id, from the models.dev
/// catalog: `None` in a slot means the catalog has no such entry.
fn limits_for(id: &str) -> (Option<u32>, Option<u32>) {
    match crate::models_dev::metadata_for("amazon-bedrock", id) {
        Some(meta) => (Some(meta.context), Some(meta.output)),
        None => (None, None),
    }
}

/// The base model id a profile's `model_arn` names
/// (`arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-…-v1:0`
/// → `anthropic.claude-…-v1:0`).
fn underlying_model(summary: &aws_sdk_bedrock::types::InferenceProfileSummary) -> Option<String> {
    let arn = summary.models().first()?.model_arn()?;
    arn.rsplit('/').next().map(str::to_owned)
}

/// A control-plane client on the same configuration the data-plane client
/// uses: the SDK default chain plus the provider's `region`/`profile`
/// overrides.
async fn client(config: &ProviderConfig) -> Client {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(profile) = &config.profile {
        loader = loader.profile_name(profile.clone());
    }
    if let Some(region) = &config.region {
        loader = loader.region(aws_config::Region::new(region.clone()));
    }
    Client::new(&loader.load().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_bedrock::types::InferenceProfileModel;
    use aws_sdk_bedrock::types::builders::InferenceProfileSummaryBuilder;

    fn summary(id: &str, arn: Option<&str>) -> aws_sdk_bedrock::types::InferenceProfileSummary {
        let mut builder = InferenceProfileSummaryBuilder::default()
            .inference_profile_id(id)
            .inference_profile_name("Cross-region Opus")
            .inference_profile_arn(format!("arn:aws:bedrock:us-east-1::inference-profile/{id}"))
            .status(InferenceProfileStatus::Active)
            .r#type(aws_sdk_bedrock::types::InferenceProfileType::SystemDefined);
        let models = match arn {
            Some(arn) => vec![InferenceProfileModel::builder().model_arn(arn).build()],
            None => vec![],
        };
        builder = builder.set_models(Some(models));
        builder.build().unwrap()
    }

    #[test]
    fn profile_maps_id_name_and_description() {
        let model = profile_to_model(&summary("us.anthropic.claude-opus-5-5", None))
            .expect("a model for a nonempty id");
        assert_eq!(model.id, "us.anthropic.claude-opus-5-5");
        assert_eq!(model.name.as_deref(), Some("Cross-region Opus"));
        assert_eq!(model.owned_by.as_deref(), Some("aws"));
    }

    #[test]
    fn empty_profile_id_is_skipped() {
        assert!(profile_to_model(&summary("", None)).is_none());
    }

    #[test]
    fn underlying_model_comes_from_the_profile_model_arn() {
        let base = underlying_model(&summary(
            "us.anthropic.claude-opus-5-5",
            Some("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-opus-5-5-v1:0"),
        ));
        assert_eq!(base.as_deref(), Some("anthropic.claude-opus-5-5-v1:0"));
    }

    #[test]
    fn missing_model_arn_yields_no_underlying_model() {
        assert!(underlying_model(&summary("us.anthropic.claude-opus-5-5", None)).is_none());
    }

    /// Discovery-time catalog lookups are the regression path: a cold
    /// models.dev catalog leaves every profile without limits, so these
    /// prove the listing reads a warmed one.
    #[test]
    fn limits_come_from_the_catalog_by_exact_profile_id() {
        let _guard = crate::models_dev::testing::GLOBAL_LOCK.blocking_lock();
        let previous = crate::models_dev::testing::install(
            r#"{"amazon-bedrock": {"models": {
                "us.anthropic.claude-sonnet-4-6": {"limit": {"context": 200000, "output": 16000}}
            }}}"#,
        );
        let model = profile_to_model(&summary("us.anthropic.claude-sonnet-4-6", None))
            .expect("a model for a nonempty id");
        assert_eq!(model.context_length, Some(200_000));
        assert_eq!(model.max_output_tokens, Some(16_000));
        crate::models_dev::testing::restore(previous);
    }

    #[test]
    fn limits_fall_back_to_the_underlying_model_when_the_profile_is_unknown() {
        let _guard = crate::models_dev::testing::GLOBAL_LOCK.blocking_lock();
        let previous = crate::models_dev::testing::install(
            r#"{"amazon-bedrock": {"models": {
                "anthropic.claude-opus-5-5-v1:0": {"limit": {"context": 1000000, "output": 128000}}
            }}}"#,
        );
        let model = profile_to_model(&summary(
            "us.anthropic.claude-opus-5-5",
            Some("arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-opus-5-5-v1:0"),
        ))
        .expect("a model for a nonempty id");
        assert_eq!(model.context_length, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(128_000));
        crate::models_dev::testing::restore(previous);
    }
}

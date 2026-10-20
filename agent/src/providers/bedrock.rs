//! Bedrock AWS configuration and model discovery.
//!
//! One craft-owned `SdkConfig` backs both Bedrock planes: the
//! control-plane client that lists inference profiles, and the data-plane
//! runtime rig-bedrock transports Converse wires through. Building it
//! ourselves instead of delegating to the SDK's (or rig's) lazy default
//! load keeps the default credential chain — environment variables,
//! `~/.aws` profiles (including SSO, assume-role and `credential_process`,
//! and the `AWS_PROFILE` the user exported), web identity tokens, ECS
//! container credentials, and finally EC2 instance metadata — while fixing
//! how it behaves off EC2:
//!
//! - The IMDS probe gets fast-fail timeouts (500ms connect, single
//!   attempt). With SDK defaults the *region* chain probes IMDS before
//!   credentials are ever consulted, stalling every cold provider build
//!   for seconds on non-EC2 machines that configured no region.
//! - A missing region fails at build time with an actionable message
//!   instead of surfacing as an opaque "dispatch failure" on the first
//!   request.
//! - Failure messages carry the SDK error's full cause chain, so an
//!   unresolved profile or an aborted credential chain says so instead
//!   of displaying only the SDK's bare "dispatch failure".
//!
//! `AWS_EC2_METADATA_DISABLED=true` skips the probe entirely (the SDK
//! honors it through the same region/credential chains). Resolved
//! configurations are deliberately not cached process-wide: everything
//! the resolution reads (env vars, `~/.aws`, the active profile) can
//! change between builds, and a shared cache could pin one
//! environment's answer onto another.

use std::sync::Arc;
use std::time::Duration;

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_bedrock::Client;
use aws_sdk_bedrock::types::InferenceProfileStatus;
use rig_core::model::{ModelInfo, ModelList};

use super::Timeouts;
use crate::config::ProviderConfig;
use crate::error::Result;

/// Tight timeouts for the EC2 IMDS credential and region probes. On
/// non-EC2 machines the default IMDS client retries with 1-second
/// timeouts, stalling every cold provider build for multiple seconds
/// before the request ever fails. These values keep the probe fast while
/// still working on EC2, where IMDS answers in milliseconds.
const IMDS_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const IMDS_READ_TIMEOUT: Duration = Duration::from_millis(500);
const IMDS_OPERATION_TIMEOUT: Duration = Duration::from_millis(800);
const IMDS_MAX_ATTEMPTS: u32 = 1;

/// The resolved AWS settings for the provider's craft configuration.
/// Loading is cheap and deliberately not cached process-wide: everything
/// the resolution reads — env vars, `~/.aws`, the active profile — can
/// change between builds, so a shared cache could pin one environment's
/// answer onto another. With a configured region the chain just parses
/// the shared files; only an entirely unconfigured machine pays the
/// tuned (sub-second, single-attempt) IMDS probe.
pub(crate) async fn sdk_config(
    config: &ProviderConfig,
    timeouts: Timeouts,
) -> Result<Arc<aws_config::SdkConfig>> {
    Ok(Arc::new(load_sdk_config(config, timeouts).await?))
}

/// [`sdk_config`] for craft's synchronous provider builders: block the
/// calling context until the configuration resolves. On the app's
/// multi-thread runtime the future joins the current handle; a
/// current-thread runtime (async tests) or no runtime at all runs it on a
/// private thread, because parking either would deadlock or panic.
pub(crate) fn sdk_config_blocking(
    config: &ProviderConfig,
    timeouts: Timeouts,
) -> Result<Arc<aws_config::SdkConfig>> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(sdk_config(config, timeouts)))
        }
        other => {
            // Swallow the handle (current-thread runtime or none at all):
            // parking either would deadlock or panic, so the load runs on a
            // private thread and the result crosses back by join.
            let _ = other;
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(crate::error::client_error)?
                            .block_on(sdk_config(config, timeouts))
                    })
                    .join()
                    .map_err(|_| {
                        crate::error::Error::Invalid {
                            // The load never panics by design; reached only
                            // on thread or runtime construction failure.
                            reason: "bedrock: AWS configuration load thread failed".to_owned(),
                        }
                    })?
            })
        }
    }
}

/// Resolve one AWS configuration: profile and region overrides from the
/// provider config, the SDK default credential chain on a fast-failing
/// IMDS client, and craft's connect/stall timeout policy. Credentials stay
/// provider objects — resolved lazily on the first signed request, so
/// building this never validates keys and remains offline.
async fn load_sdk_config(
    config: &ProviderConfig,
    timeouts: Timeouts,
) -> Result<aws_config::SdkConfig> {
    let provider_config = aws_config::provider_config::ProviderConfig::without_region();
    let imds_client = aws_config::imds::Client::builder()
        .configure(&provider_config)
        .connect_timeout(IMDS_CONNECT_TIMEOUT)
        .read_timeout(IMDS_READ_TIMEOUT)
        .operation_timeout(IMDS_OPERATION_TIMEOUT)
        .max_attempts(IMDS_MAX_ATTEMPTS)
        .build();

    // Region: the craft config's `region` wins; otherwise the SDK
    // default chain (AWS_REGION/AWS_DEFAULT_REGION, the named or
    // AWS_PROFILE profile's region, then IMDS) — with the fast client, so
    // the IMDS fallback cannot stall a cold build for seconds. The
    // credentials chain and the config loader each take a fresh chain.

    // The full SDK default chain (environment, profile files with SSO
    // and credential_process support, web identity, ECS, IMDS last) with
    // the same fast IMDS client; craft's `profile` overrides the same
    // way `AWS_PROFILE` would, scoped to this provider.
    let mut credentials_builder =
        aws_config::default_provider::credentials::DefaultCredentialsChain::builder()
            .configure(provider_config.clone())
            .imds_client(imds_client.clone())
            .region(region_chain(config, &provider_config, imds_client.clone()));
    if let Some(profile) = &config.profile {
        credentials_builder = credentials_builder.profile_name(profile);
    }
    let credentials_provider = credentials_builder.build().await;

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .credentials_provider(credentials_provider);
    if let Some(profile) = &config.profile {
        loader = loader.profile_name(profile.clone());
    }
    loader = match &config.region {
        Some(region) => loader.region(aws_config::Region::new(region.clone())),
        None => loader.region(region_chain(config, &provider_config, imds_client)),
    };
    let sdk_config = loader.load().await;

    if sdk_config.region().is_none() {
        return Err(crate::error::Error::Invalid {
            reason: "no AWS region resolved: set `region` on the provider, \
                     AWS_REGION/AWS_DEFAULT_REGION, or a `region` in the active AWS profile"
                .to_owned(),
        });
    }

    // AWS owns the transport, so the shared `Timeouts` policy maps onto
    // the SDK: quick connect failures and a stall floor between reads.
    // No overall operation timeout — a long ConverseStream turn must not
    // be cut off by the same policy that bounds whole single responses.
    let timeout_config = aws_smithy_types::timeout::TimeoutConfig::builder()
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.low_speed)
        .build();

    Ok(sdk_config
        .to_builder()
        .timeout_config(timeout_config)
        .build())
}

/// The SDK default region chain — environment, profile file, IMDS — with
/// the fast IMDS client swapped in so the fallback probe fails quickly on
/// non-EC2 machines. A fresh chain per call: the credentials chain and
/// the config loader each consume one.
fn region_chain(
    config: &ProviderConfig,
    provider_config: &aws_config::provider_config::ProviderConfig,
    imds_client: aws_config::imds::Client,
) -> RegionProviderChain {
    RegionProviderChain::first_try(
        aws_config::environment::region::EnvironmentVariableRegionProvider::new(),
    )
    .or_else(region_from_profile(config, provider_config))
    .or_else(
        aws_config::imds::region::Builder::default()
            .configure(provider_config)
            .imds_client(imds_client)
            .build(),
    )
}

/// The profile-file leg of the region chain, honoring the craft config's
/// `profile`, AWS_PROFILE, and the shared config files' default profile.
fn region_from_profile(
    config: &ProviderConfig,
    provider_config: &aws_config::provider_config::ProviderConfig,
) -> aws_config::profile::region::ProfileFileRegionProvider {
    let mut builder = aws_config::profile::region::ProfileFileRegionProvider::builder()
        .configure(provider_config);
    if let Some(profile) = &config.profile {
        builder = builder.profile_name(profile);
    }
    builder.build()
}

/// The error text for an AWS SDK failure with its cause chain intact.
/// The SDK's `Display` is only "dispatch failure"; the reasons a user can
/// act on (profile not found, SSO token missing, IMDS unreachable, ...)
/// live in the sources, so surface them all, dropping repeats.
pub(crate) fn aws_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(err) = source {
        let text = err.to_string();
        if !text.is_empty() && parts.last().is_none_or(|last| *last != *text) {
            parts.push(text);
        }
        source = err.source();
    }
    parts.join(" - ")
}

/// List the ACTIVE inference profiles as rig models, enriched with
/// context/output limits from the models.dev catalog. Both listing types
/// are included: system profiles (the `us.`/`eu.` cross-region spellings)
/// and application profiles the account defined.
pub(crate) async fn list_models(config: &ProviderConfig, timeouts: Timeouts) -> Result<ModelList> {
    let client = client(config, timeouts).await?;
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
                reason: format!("bedrock discovery failed: {}", aws_error_chain(&e)),
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

/// A control-plane client on the same cached configuration the
/// data-plane client uses: the SDK default chain plus the provider's
/// `region`/`profile` overrides.
async fn client(config: &ProviderConfig, timeouts: Timeouts) -> Result<Client> {
    let sdk_config = sdk_config(config, timeouts).await?;
    Ok(Client::new(&sdk_config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::provider::ProvideCredentials;
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

    // --- Credential- and region-chain behavior -------------------------
    //
    // These tests resolve real SDK configurations offline: profiles come
    // from a throwaway HOME, credentials are resolved (never signed), and
    // nothing contacts AWS. Env-mutating tests serialize on AWS_ENV_LOCK;
    // the guard is held across awaits deliberately — the loads run on the
    // test's own current-thread runtime, so no other task contends for it.

    use crate::providers::registry::ProviderKind;

    static AWS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Every environment source the AWS region/credential chains read:
    /// cleared at isolation, restored on drop.
    const AWS_ENV_VARS: &[&str] = &[
        "HOME",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_EC2_METADATA_DISABLED",
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_ROLE_ARN",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        "AWS_ENDPOINT_URL",
    ];

    /// Isolate the process environment the SDK chains read: `HOME` moves
    /// to `home`, every other known AWS source is cleared, and the pinned
    /// pairs land last. Everything restores on drop.
    struct AwsEnv {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl AwsEnv {
        fn isolate(home: &std::path::Path, pinned: &[(&'static str, &str)]) -> Self {
            let mut saved: Vec<(&'static str, Option<String>)> = Vec::new();
            for key in AWS_ENV_VARS {
                saved.push((key, std::env::var(key).ok()));
                // SAFETY: test-only env mutation, serialized by AWS_ENV_LOCK.
                unsafe { std::env::remove_var(key) };
            }
            unsafe { std::env::set_var("HOME", home) };
            for (key, value) in pinned {
                unsafe { std::env::set_var(key, value) };
            }
            Self { saved }
        }
    }

    impl Drop for AwsEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                // SAFETY: restoring what this guard mutated.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn bedrock_config(region: Option<&str>, profile: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: ProviderKind::Bedrock,
            api_key_env: None,
            base_url: None,
            api_version: None,
            account_id: None,
            region: region.map(str::to_owned),
            profile: profile.map(str::to_owned),
            discover_models: true,
            models: Default::default(),
        }
    }

    /// A throwaway HOME whose `testbed` profile carries static fake keys
    /// and a region: the documented ~/.aws layout, with nothing real.
    fn fake_aws_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let aws = home.path().join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        std::fs::write(
            aws.join("config"),
            "[profile testbed]\nregion = us-west-2\n",
        )
        .unwrap();
        std::fs::write(
            aws.join("credentials"),
            "[testbed]\naws_access_key_id = AKIAFAKETESTBED00099\n\
             aws_secret_access_key = fake-secret-never-valid-00099\n",
        )
        .unwrap();
        home
    }

    #[tokio::test]
    async fn repeated_loads_resolve_the_same_region() {
        let config = bedrock_config(Some("ap-southeast-7"), None);
        let first = sdk_config(&config, Timeouts::default()).await.unwrap();
        assert_eq!(
            first.region().map(|r| r.to_string()),
            Some("ap-southeast-7".to_owned())
        );
        let second = sdk_config(&config, Timeouts::default()).await.unwrap();
        assert_eq!(
            second.region().map(|r| r.to_string()),
            Some("ap-southeast-7".to_owned())
        );
    }

    /// `block_in_place` — the naive sync bridge — panics on current-thread
    /// runtimes, which is exactly where async tests build providers. This
    /// run is the regression guard for the thread-fallback bridge.
    #[tokio::test]
    async fn blocking_load_survives_a_current_thread_runtime() {
        let config = bedrock_config(Some("ap-southeast-8"), None);
        let sdk_config = sdk_config_blocking(&config, Timeouts::default()).unwrap();
        assert_eq!(
            sdk_config.region().map(|r| r.to_string()),
            Some("ap-southeast-8".to_owned())
        );
    }

    /// The pre-fix behavior on a non-EC2 machine with no region configured
    /// anywhere: seconds of IMDS connect timeouts, then an opaque
    /// "dispatch failure". Now the region guard fails fast with the fixes
    /// spelled out, and the tuned IMDS probe keeps the failure quick.
    #[tokio::test]
    async fn missing_region_fails_fast_with_the_fixes_spelled_out() {
        let _lock = AWS_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _env = AwsEnv::isolate(home.path(), &[]);
        let config = bedrock_config(None, None);
        let started = std::time::Instant::now();
        let error = sdk_config(&config, Timeouts::default()).await.unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("no AWS region resolved"), "{message}");
        assert!(message.contains("AWS_REGION"), "{message}");
        // The IMDS fallback is tuned to fail in well under the SDK's
        // default multi-second stall (3 probes x 1s connect + retries).
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "region probe stalled for {:?}",
            started.elapsed()
        );
    }

    /// The named-profile support the provider config documents: credentials
    /// and region come from the profile in the shared config files, and
    /// resolution never leaves the machine (credentials are produced, not
    /// signed).
    #[tokio::test]
    async fn named_profile_resolves_credentials_and_region_offline() {
        let _lock = AWS_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = fake_aws_home();
        let _env = AwsEnv::isolate(home.path(), &[]);
        let config = bedrock_config(None, Some("testbed"));
        let sdk_config = sdk_config(&config, Timeouts::default()).await.unwrap();
        assert_eq!(
            sdk_config.region().map(|r| r.to_string()),
            Some("us-west-2".to_owned()),
            "the profile's region must resolve without IMDS"
        );
        let credentials = sdk_config
            .credentials_provider()
            .expect("the default chain is attached")
            .provide_credentials()
            .await
            .expect("the named profile carries the credentials");
        assert!(credentials.access_key_id().starts_with("AKIAFAKETESTBED"));
    }

    /// The user-facing way to select a profile without craft config: set
    /// `AWS_PROFILE` in the environment. The SDK chains must honor it for
    /// both credentials and region — the "AWS_PROFILE doesn't work off
    /// EC2" regression.
    #[tokio::test]
    async fn aws_profile_env_selects_the_shared_config_profile() {
        let _lock = AWS_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = fake_aws_home();
        let _env = AwsEnv::isolate(home.path(), &[("AWS_PROFILE", "testbed")]);
        let config = bedrock_config(None, None);
        let sdk_config = sdk_config(&config, Timeouts::default()).await.unwrap();
        assert_eq!(
            sdk_config.region().map(|r| r.to_string()),
            Some("us-west-2".to_owned()),
            "AWS_PROFILE drives the region chain natively"
        );
        let credentials = sdk_config
            .credentials_provider()
            .expect("the default chain is attached")
            .provide_credentials()
            .await
            .expect("the AWS_PROFILE credentials resolve");
        assert!(credentials.access_key_id().starts_with("AKIAFAKETESTBED"));
    }
}

//! Amazon Bedrock provider backed by the official AWS SDK for Rust.
//!
//! Data plane uses `aws_sdk_bedrockruntime` `ConverseStream`; control plane
//! uses `aws_sdk_bedrock` `ListInferenceProfiles` for model discovery. Auth is
//! the full AWS SDK credential chain (env, profiles, SSO, IMDS, web identity,
//! container creds) via `aws_config::load_defaults`.

use aws_config::BehaviorVersion;
use aws_sdk_bedrock::Client as BedrockClient;
use aws_sdk_bedrockruntime::Client as RuntimeClient;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_smithy_types::timeout::TimeoutConfig;
use flume::Sender;
use tracing::{debug, warn};

use crate::AgentError;
use crate::model::{Model, ModelInfo, TokenUsage};
use crate::provider::{BoxFuture, Provider};
use crate::types::{Message, ProviderEvent, RequestOptions, Role, StreamResponse};

use super::Timeouts;

mod converse;
mod stream;

pub(crate) use converse::{inference_config, system_block, to_aws_messages, to_aws_tools};

pub struct Bedrock {
    rt: RuntimeClient,
    ctrl: BedrockClient,
    region: String,
}

// Submitted unconditionally so `craft auth status` shows a Bedrock row. The
// `Protocol::Anthropic` is a placeholder: Bedrock bypasses the protocol shim
// (it uses the SDK, not a wire format), and no consumer of `protocol` reaches
// this entry.
inventory::submit!(craft_config::providers::BuiltInProvider {
    slug: "bedrock",
    display_name: "Bedrock",
    protocol: craft_config::providers::Protocol::Anthropic,
    default_base_url: "",
    default_api_key_env: "AWS_REGION",
    default_model: "bedrock/us.anthropic.claude-sonnet-4-6-20250514-v1:0",
    plans: None,
    login_url: Some("https://docs.aws.amazon.com/bedrock/latest/userguide/setting-up.html"),
    needs_url: false,
});

/// Constructs a [`Bedrock`] provider or returns a config error when the feature
/// is disabled. Exposed as a free fn so `ProviderKind::Bedrock::create` (which
/// must compile without the feature) can call it via the cfg-gated module.
pub(crate) async fn create(timeouts: Timeouts) -> Result<Bedrock, AgentError> {
    Bedrock::new(timeouts).await
}

impl Bedrock {
    pub(crate) async fn new(timeouts: Timeouts) -> Result<Self, AgentError> {
        let sdk_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
        Self::from_config(sdk_config, timeouts)
    }

    fn from_config(
        sdk_config: aws_config::SdkConfig,
        timeouts: Timeouts,
    ) -> Result<Self, AgentError> {
        let region = sdk_config
            .region()
            .map(|r| r.as_ref().to_string())
            .unwrap_or_else(|| {
                std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string())
            });
        let timeout_config = TimeoutConfig::builder()
            .connect_timeout(timeouts.connect)
            .read_timeout(timeouts.low_speed)
            .operation_timeout(timeouts.stream)
            .build();
        let sdk_config = sdk_config
            .to_builder()
            .timeout_config(timeout_config)
            .build();
        let rt = RuntimeClient::new(&sdk_config);
        let ctrl = BedrockClient::new(&sdk_config);
        debug!(region = %region, "bedrock provider initialized");
        Ok(Self { rt, ctrl, region })
    }

    async fn list_inference_profiles(&self) -> Result<Vec<ModelInfo>, AgentError> {
        use aws_sdk_bedrock::types::InferenceProfileStatus;
        use aws_sdk_bedrock::types::InferenceProfileType;

        let mut models: Vec<ModelInfo> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for profile_type in [
            InferenceProfileType::SystemDefined,
            InferenceProfileType::Application,
        ] {
            let mut paginator = self
                .ctrl
                .list_inference_profiles()
                .type_equals(profile_type)
                .into_paginator()
                .send();
            while let Some(resp) = paginator.next().await {
                let resp = resp.map_err(|e| {
                    let code = e.code().unwrap_or("");
                    AgentError::Api {
                        status: status_for_code(code).unwrap_or(502),
                        message: e
                            .message()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("{e}")),
                    }
                })?;
                for summary in resp.inference_profile_summaries() {
                    if summary.status != InferenceProfileStatus::Active {
                        continue;
                    }
                    let id = summary.inference_profile_id();
                    let name = summary.inference_profile_name();
                    let model_id = if id.is_empty() {
                        name.to_string()
                    } else {
                        id.to_string()
                    };
                    if model_id.is_empty() || !seen.insert(model_id.clone()) {
                        continue;
                    }
                    models.push(ModelInfo::new(model_id));
                }
            }
        }

        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

impl Provider for Bedrock {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a serde_json::Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let aws_messages = to_aws_messages(messages)?;
            let aws_tools = to_aws_tools(tools);
            let system_blocks = vec![system_block(system)];
            let inference = inference_config(model, &opts);

            let mut builder = self
                .rt
                .converse_stream()
                .model_id(&model.id)
                .set_messages(Some(aws_messages));
            for sb in system_blocks {
                builder = builder.system(sb);
            }
            if let Some(tc) = aws_tools {
                builder = builder.tool_config(tc);
            }
            builder = builder.inference_config(inference);

            debug!(model = %model.id, region = %self.region, "sending Bedrock ConverseStream");

            let resp = builder.send().await.map_err(map_send_error)?;
            // Drive the SDK EventReceiver inline: its type lives in the SDK's
            // private `event_receiver` module, so we never name it; we call
            // `.recv()` on the value from `resp.stream` and delegate per-event
            // assembly to the pure, tested `stream::process_event`.
            use crate::types::{ContentBlock, StopReason};
            let mut receiver = resp.stream;
            let mut content_blocks: Vec<ContentBlock> = Vec::new();
            let mut block_states: Vec<stream::BlockState> = Vec::new();
            let mut usage = TokenUsage::default();
            let mut stop_reason: Option<StopReason> = None;
            loop {
                let event = match receiver.recv().await {
                    Ok(Some(ev)) => ev,
                    Ok(None) => break,
                    Err(err) => return Err(stream::map_recv_error(err)),
                };
                stream::process_event(
                    &event,
                    &mut content_blocks,
                    &mut block_states,
                    &mut usage,
                    &mut stop_reason,
                    event_tx,
                )
                .await?;
                if stop_reason.is_some() {
                    break;
                }
            }
            Ok(StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: content_blocks,
                    ..Default::default()
                },
                usage,
                stop_reason,
            })
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(async {
            let models = self.list_inference_profiles().await?;
            Ok(models.into_iter().map(|m| m.id).collect())
        })
    }

    fn list_models_with_info(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
        Box::pin(self.list_inference_profiles())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        // The AWS SDK refreshes short-lived credentials (SSO, IMDS, web identity)
        // automatically via its credentials cache between requests. Region and
        // profile changes, however, are baked into the clients at construction
        // and require a process restart to take effect.
        Box::pin(async {
            debug!("bedrock reload_auth: no-op (SDK manages credential refresh)");
            Ok(())
        })
    }
}

/// Maps an AWS SDK `ConverseStream` send error to `AgentError::Api` with a
/// best-effort HTTP status, so the agent's throttling / auth retry heuristics
/// (`should_rotate_key`) fire for 429/401/403.
fn map_send_error<E>(
    err: SdkError<E, aws_smithy_runtime_api::client::orchestrator::HttpResponse>,
) -> AgentError
where
    E: ProvideErrorMetadata,
{
    let message = err
        .message()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{err}"));
    let code = err.code().unwrap_or("");
    let status = status_for_code(code).unwrap_or_else(|| {
        err.raw_response()
            .map(|r| r.status().as_u16())
            .unwrap_or(500)
    });
    warn!(code, status, message = %message, "bedrock converse_stream failed");
    AgentError::Api { status, message }
}

fn status_for_code(code: &str) -> Option<u16> {
    match code {
        "ThrottlingException" | "TooManyRequestsException" => Some(429),
        "AccessDeniedException" | "ForbiddenException" => Some(403),
        "UnrecognizedClientException" | "UnauthorizedException" => Some(401),
        "ValidationException" | "ResourceNotFoundException" => Some(400),
        "ModelNotReadyException" => Some(409),
        "InternalServerException" | "ServiceUnavailableException" => Some(500),
        _ => None,
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod tests {
    use super::*;
    use aws_sdk_bedrock::types::{
        InferenceProfileStatus, InferenceProfileSummary, InferenceProfileType,
    };
    use aws_sdk_bedrockruntime::types::StopReason as AwsStopReason;

    #[test]
    fn status_for_code_maps_throttling_and_auth() {
        assert_eq!(status_for_code("ThrottlingException"), Some(429));
        assert_eq!(status_for_code("AccessDeniedException"), Some(403));
        assert_eq!(status_for_code("UnrecognizedClientException"), Some(401));
        assert_eq!(status_for_code("ValidationException"), Some(400));
        assert_eq!(status_for_code("UnknownWeirdError"), None);
    }

    #[test]
    fn stop_reason_mapping_matches_aws_variants() {
        use crate::types::StopReason;
        assert_eq!(
            map_stop_reason(&AwsStopReason::EndTurn),
            StopReason::EndTurn
        );
        assert_eq!(
            map_stop_reason(&AwsStopReason::ToolUse),
            StopReason::ToolUse
        );
        assert_eq!(
            map_stop_reason(&AwsStopReason::MaxTokens),
            StopReason::MaxTokens
        );
    }

    #[test]
    fn summaries_to_model_info_dedupes_active_profiles() {
        let summaries = vec![
            InferenceProfileSummary::builder()
                .inference_profile_id("anthropic.claude-sonnet-4-6-20250514-v1:0")
                .inference_profile_name("Claude Sonnet 4.6")
                .inference_profile_arn("arn:aws:bedrock:us-east-1::profile/1")
                .status(InferenceProfileStatus::Active)
                .set_models(Some(vec![]))
                .r#type(InferenceProfileType::SystemDefined)
                .build()
                .unwrap(),
            InferenceProfileSummary::builder()
                .inference_profile_id("us.anthropic.claude-opus-4-1-v1:0")
                .inference_profile_name("Claude Opus 4.1 cross-region")
                .inference_profile_arn("arn:aws:bedrock:us-east-1::profile/2")
                .status(InferenceProfileStatus::Active)
                .set_models(Some(vec![]))
                .r#type(InferenceProfileType::SystemDefined)
                .build()
                .unwrap(),
            // Duplicate of the first: must be dropped by dedup.
            InferenceProfileSummary::builder()
                .inference_profile_id("anthropic.claude-sonnet-4-6-20250514-v1:0")
                .inference_profile_name("Claude Sonnet 4.6 (dup)")
                .inference_profile_arn("arn:aws:bedrock:us-east-1::profile/1b")
                .status(InferenceProfileStatus::Active)
                .set_models(Some(vec![]))
                .r#type(InferenceProfileType::SystemDefined)
                .build()
                .unwrap(),
        ];
        let models = summaries_to_model_info(&summaries);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "anthropic.claude-sonnet-4-6-20250514-v1:0",
                "us.anthropic.claude-opus-4-1-v1:0",
            ]
        );
    }

    #[test]
    fn summaries_fall_back_to_name_when_id_empty() {
        let summaries = vec![
            InferenceProfileSummary::builder()
                .inference_profile_id("")
                .inference_profile_name("by-name-profile")
                .inference_profile_arn("arn:aws:bedrock:us-east-1::profile/4")
                .status(InferenceProfileStatus::Active)
                .set_models(Some(vec![]))
                .r#type(InferenceProfileType::SystemDefined)
                .build()
                .unwrap(),
        ];
        let models = summaries_to_model_info(&summaries);
        assert_eq!(models[0].id, "by-name-profile");
    }

    fn summaries_to_model_info(summaries: &[InferenceProfileSummary]) -> Vec<ModelInfo> {
        let mut models: Vec<ModelInfo> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for s in summaries {
            if s.status != InferenceProfileStatus::Active {
                continue;
            }
            let id = if s.inference_profile_id.is_empty() {
                s.inference_profile_name.clone()
            } else {
                s.inference_profile_id.clone()
            };
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            models.push(ModelInfo::new(id));
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
}

/// Maps an AWS `StopReason` to craft's normalized [`StopReason`]. Lives here
/// (rather than in `stream.rs`) so the unit test can construct AWS variants
/// without a stream loop.
pub(crate) fn map_stop_reason(
    reason: &aws_sdk_bedrockruntime::types::StopReason,
) -> crate::types::StopReason {
    use crate::types::StopReason;
    use aws_sdk_bedrockruntime::types::StopReason as R;
    match reason {
        R::EndTurn => StopReason::EndTurn,
        R::ToolUse => StopReason::ToolUse,
        R::MaxTokens => StopReason::MaxTokens,
        R::StopSequence | R::GuardrailIntervened | R::ContentFiltered => StopReason::EndTurn,
        other => {
            warn!(?other, "unmapped AWS StopReason, defaulting to EndTurn");
            StopReason::EndTurn
        }
    }
}

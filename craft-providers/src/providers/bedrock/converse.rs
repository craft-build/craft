//! Pure translation between craft's normalized message/tool types and the
//! AWS Bedrock Runtime `ConverseStream` types. No I/O here, so the fns are
//! unit-testable without credentials.

use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration,
    Message as AwsMessage, ReasoningContentBlock, ReasoningTextBlock, SystemContentBlock, Tool,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolSpecification,
    ToolUseBlock,
};
use aws_smithy_types::{Blob, Document};
use base64::Engine;
use serde_json::Value;
use tracing::warn;

use crate::AgentError;
use crate::model::Model;
use crate::types::{ContentBlock as CraftBlock, ImageMediaType, Message, RequestOptions, Role};

const FALLBACK_MAX_TOKENS: i32 = 4_096;

pub(crate) fn system_block(system: &str) -> SystemContentBlock {
    SystemContentBlock::Text(system.to_string())
}

pub(crate) fn to_aws_messages(messages: &[Message]) -> Result<Vec<AwsMessage>, AgentError> {
    let mut out = Vec::with_capacity(messages.len());
    let mut known_tool_use_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => ConversationRole::User,
            Role::Assistant => ConversationRole::Assistant,
        };
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(msg.content.len());
        for block in &msg.content {
            if let CraftBlock::ToolResult { tool_use_id, .. } = block
                && !known_tool_use_ids.contains(tool_use_id.as_str())
            {
                warn!(
                    tool_use_id = %tool_use_id,
                    "bedrock dropping orphan tool_result with no matching tool_use"
                );
                continue;
            }
            blocks.push(to_aws_block(block)?);
            if let CraftBlock::ToolUse { id, .. } = block {
                known_tool_use_ids.insert(id.as_str());
            }
        }
        if blocks.is_empty() {
            blocks.push(ContentBlock::Text(String::new()));
        }
        out.push(
            AwsMessage::builder()
                .role(role)
                .set_content(Some(blocks))
                .build()
                .map_err(|e| AgentError::Config {
                    message: format!("bedrock message build: {e}"),
                })?,
        );
    }
    Ok(out)
}

fn to_aws_block(block: &CraftBlock) -> Result<ContentBlock, AgentError> {
    Ok(match block {
        CraftBlock::Text { text } => ContentBlock::Text(text.clone()),
        CraftBlock::ToolUse {
            id, name, input, ..
        } => ContentBlock::ToolUse(
            ToolUseBlock::builder()
                .tool_use_id(id)
                .name(name)
                .input(value_to_document(input))
                .build()
                .map_err(|e| AgentError::Config {
                    message: format!("bedrock tool use build: {e}"),
                })?,
        ),
        CraftBlock::ToolResult {
            tool_use_id,
            content,
            images,
            is_error,
        } => {
            let mut result_blocks: Vec<ToolResultContentBlock> =
                Vec::with_capacity(1 + images.len());
            if !content.is_empty() {
                result_blocks.push(ToolResultContentBlock::Text(content.clone()));
            }
            for img in images {
                let bytes = decode_b64(&img.data)?;
                let format = match img.media_type {
                    ImageMediaType::Png => ImageFormat::Png,
                    ImageMediaType::Jpeg => ImageFormat::Jpeg,
                    ImageMediaType::Gif => ImageFormat::Gif,
                    ImageMediaType::Webp => ImageFormat::Webp,
                };
                result_blocks.push(ToolResultContentBlock::Image(
                    ImageBlock::builder()
                        .format(format)
                        .source(ImageSource::Bytes(Blob::new(bytes)))
                        .build()
                        .map_err(|e| AgentError::Config {
                            message: format!("bedrock image build: {e}"),
                        })?,
                ));
            }
            if result_blocks.is_empty() {
                result_blocks.push(ToolResultContentBlock::Text(String::new()));
            }
            let mut builder = ToolResultBlock::builder().tool_use_id(tool_use_id);
            for rb in &result_blocks {
                builder = builder.content(rb.clone());
            }
            if *is_error {
                builder = builder.status(aws_sdk_bedrockruntime::types::ToolResultStatus::Error);
            }
            ContentBlock::ToolResult(builder.build().map_err(|e| AgentError::Config {
                message: format!("bedrock tool result build: {e}"),
            })?)
        }
        CraftBlock::Image { source } => {
            let bytes = decode_b64(&source.data)?;
            let format = match source.media_type {
                ImageMediaType::Png => ImageFormat::Png,
                ImageMediaType::Jpeg => ImageFormat::Jpeg,
                ImageMediaType::Gif => ImageFormat::Gif,
                ImageMediaType::Webp => ImageFormat::Webp,
            };
            ContentBlock::Image(
                ImageBlock::builder()
                    .format(format)
                    .source(ImageSource::Bytes(Blob::new(bytes)))
                    .build()
                    .map_err(|e| AgentError::Config {
                        message: format!("bedrock image build: {e}"),
                    })?,
            )
        }
        CraftBlock::Thinking {
            thinking,
            signature,
        } => ContentBlock::ReasoningContent(ReasoningContentBlock::ReasoningText(
            ReasoningTextBlock::builder()
                .set_text(Some(thinking.clone()))
                .set_signature(signature.clone())
                .build()
                .map_err(|e| AgentError::Config {
                    message: format!("bedrock reasoning build: {e}"),
                })?,
        )),
        CraftBlock::RedactedThinking { data } => {
            let bytes = decode_b64(data)?;
            ContentBlock::ReasoningContent(ReasoningContentBlock::RedactedContent(Blob::new(bytes)))
        }
    })
}

fn decode_b64(data: &str) -> Result<Vec<u8>, AgentError> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| AgentError::Config {
            message: format!("bedrock base64 decode: {e}"),
        })
}

pub(crate) fn to_aws_tools(tools: &Value) -> Option<ToolConfiguration> {
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut specs: Vec<Tool> = Vec::with_capacity(arr.len());
    for def in arr {
        let name = match def.get("name").and_then(Value::as_str) {
            Some(n) => n,
            None => {
                warn!(?def, "bedrock tool missing name, skipping");
                continue;
            }
        };
        let description = def.get("description").and_then(Value::as_str);
        let schema_value = def.get("input_schema").cloned().unwrap_or(Value::Null);
        let mut spec_builder = ToolSpecification::builder().name(name);
        if let Some(d) = description {
            spec_builder = spec_builder.description(d);
        }
        spec_builder =
            spec_builder.input_schema(ToolInputSchema::Json(value_to_document(&schema_value)));
        match spec_builder.build() {
            Ok(spec) => specs.push(Tool::ToolSpec(spec)),
            Err(e) => warn!(tool = name, error = %e, "bedrock tool spec build failed, skipping"),
        }
    }
    if specs.is_empty() {
        return None;
    }
    ToolConfiguration::builder()
        .set_tools(Some(specs))
        .build()
        .ok()
}

pub(crate) fn inference_config(model: &Model, opts: &RequestOptions) -> InferenceConfiguration {
    let mut builder = InferenceConfiguration::builder();
    let max_tokens = model
        .max_output_tokens
        .map(|n| i32::try_from(n).unwrap_or(FALLBACK_MAX_TOKENS))
        .unwrap_or(FALLBACK_MAX_TOKENS);
    builder = builder.max_tokens(max_tokens);
    if opts.thinking.is_enabled()
        && let crate::types::ThinkingConfig::Budget(_) = opts.thinking
    {
        builder = builder.temperature(0.0).top_p(0.0);
    }
    builder.build()
}

/// `aws_smithy_types::Document` has no public `From<serde_json::Value>` (its serde
/// impls are behind the unstable `aws_sdk_unstable` gate), so we walk the JSON
/// tree ourselves. Bedrock's `input_schema` field is a JSON schema object; the
/// rest of the input is arbitrary JSON passed as a tool argument document.
pub(crate) fn value_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else if let Some(f) = n.as_f64() {
                Document::Number(aws_smithy_types::Number::Float(f))
            } else {
                Document::Null
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(arr) => Document::Array(arr.iter().map(value_to_document).collect()),
        Value::Object(obj) => {
            let map = obj
                .iter()
                .map(|(k, v)| (k.clone(), value_to_document(v)))
                .collect();
            Document::Object(map)
        }
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod tests {
    use super::*;
    use crate::types::Role;
    use aws_sdk_bedrockruntime::types::{ContentBlock as AwsContentBlock, ConversationRole};
    use serde_json::json;
    use std::sync::Arc;
    use test_case::test_case;

    #[test]
    fn text_message_round_trip() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![CraftBlock::Text {
                text: "hello".into(),
            }],
            ..Default::default()
        }];
        let aws = to_aws_messages(&msgs).unwrap();
        assert_eq!(aws.len(), 1);
        assert_eq!(aws[0].role, ConversationRole::User);
        let block = &aws[0].content[0];
        match block {
            AwsContentBlock::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn tool_use_block_carries_input_document() {
        let input = json!({"command": "ls"});
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![CraftBlock::tool_use("tu_1", "bash", input.clone())],
            ..Default::default()
        }];
        let aws = to_aws_messages(&msgs).unwrap();
        match &aws[0].content[0] {
            AwsContentBlock::ToolUse(tu) => {
                assert_eq!(tu.tool_use_id(), "tu_1");
                assert_eq!(tu.name(), "bash");
                match tu.input() {
                    Document::Object(m) => assert!(m.contains_key("command")),
                    other => panic!("expected Object document, got {other:?}"),
                }
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_block_maps_id_and_status() {
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![CraftBlock::tool_use("tu_1", "bash", json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![CraftBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "done".into(),
                    images: vec![],
                    is_error: true,
                }],
                ..Default::default()
            },
        ];
        let aws = to_aws_messages(&msgs).unwrap();
        match &aws[1].content[0] {
            AwsContentBlock::ToolResult(tr) => {
                assert_eq!(tr.tool_use_id(), "tu_1");
                assert_eq!(
                    tr.status,
                    Some(aws_sdk_bedrockruntime::types::ToolResultStatus::Error)
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn orphan_tool_result_is_dropped() {
        // Synthetic post-write events (format-<id>, validation-<id>) and any
        // other tool_result without a matching tool_use must be omitted, or
        // Bedrock rejects the request with 400 "unexpected tool_use_id".
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![CraftBlock::tool_use("tu_real", "bash", json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    CraftBlock::ToolResult {
                        tool_use_id: "tu_real".into(),
                        content: "done".into(),
                        images: vec![],
                        is_error: false,
                    },
                    CraftBlock::ToolResult {
                        tool_use_id: "format-tu_real".into(),
                        content: "reformatted".into(),
                        images: vec![],
                        is_error: false,
                    },
                ],
                ..Default::default()
            },
        ];
        let aws = to_aws_messages(&msgs).unwrap();
        let results: Vec<&AwsContentBlock> = aws[1]
            .content
            .iter()
            .filter(|b| matches!(b, AwsContentBlock::ToolResult(_)))
            .collect();
        assert_eq!(results.len(), 1, "orphan tool_result should be dropped");
        match results[0] {
            AwsContentBlock::ToolResult(tr) => assert_eq!(tr.tool_use_id(), "tu_real"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn thinking_block_round_trip() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![CraftBlock::Thinking {
                thinking: "reasoning".into(),
                signature: Some("sig".into()),
            }],
            ..Default::default()
        }];
        let aws = to_aws_messages(&msgs).unwrap();
        match &aws[0].content[0] {
            AwsContentBlock::ReasoningContent(rc) => match rc {
                ReasoningContentBlock::ReasoningText(rt) => {
                    assert_eq!(rt.text(), "reasoning");
                    assert_eq!(rt.signature(), Some("sig"));
                }
                other => panic!("expected ReasoningText, got {other:?}"),
            },
            other => panic!("expected ReasoningContent, got {other:?}"),
        }
    }

    #[test]
    fn image_block_uses_png_format() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![CraftBlock::Image {
                source: crate::types::ImageSource::new(ImageMediaType::Png, Arc::from("AAEC")),
            }],
            ..Default::default()
        }];
        let aws = to_aws_messages(&msgs).unwrap();
        match &aws[0].content[0] {
            AwsContentBlock::Image(img) => {
                assert_eq!(img.format, ImageFormat::Png);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn to_aws_tools_builds_specs_from_json() {
        let tools = json!([
            {"name": "bash", "description": "run a command", "input_schema": {"type": "object"}},
            {"name": "read", "input_schema": {}}
        ]);
        let cfg = to_aws_tools(&tools).unwrap();
        assert_eq!(cfg.tools.len(), 2);
    }

    #[test]
    fn to_aws_tools_returns_none_for_empty_or_non_array() {
        assert!(to_aws_tools(&json!([])).is_none());
        assert!(to_aws_tools(&json!(null)).is_none());
        assert!(to_aws_tools(&json!({"x": 1})).is_none());
    }

    #[test]
    fn to_aws_tools_skips_entries_without_name() {
        let tools = json!([
            {"description": "no name", "input_schema": {}},
            {"name": "ok", "input_schema": {}}
        ]);
        let cfg = to_aws_tools(&tools).unwrap();
        assert_eq!(cfg.tools.len(), 1);
    }

    #[test_case(json!(null), "null")]
    #[test_case(json!(true), "bool")]
    #[test_case(json!(42), "int")]
    #[test_case(json!(-7), "neg_int")]
    #[test_case(json!(2.5), "float")]
    #[test_case(json!("hi"), "string")]
    #[test_case(json!([1, 2]), "array")]
    #[test_case(json!({"k": "v"}), "object")]
    fn value_to_document_handles_all_shapes(value: Value, _label: &str) {
        let doc = value_to_document(&value);
        let _ = format!("{doc:?}");
    }

    #[test]
    fn value_to_document_preserves_object_keys() {
        let value = json!({"type": "object", "properties": {"x": {"type": "integer"}}});
        let doc = value_to_document(&value);
        match doc {
            Document::Object(m) => {
                assert!(m.contains_key("type"));
                assert!(m.contains_key("properties"));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn value_to_document_routes_positive_int_to_posint() {
        let doc = value_to_document(&json!(5));
        match doc {
            Document::Number(aws_smithy_types::Number::PosInt(5)) => {}
            other => panic!("expected PosInt(5), got {other:?}"),
        }
    }

    #[test]
    fn value_to_document_routes_negative_int_to_negint() {
        let doc = value_to_document(&json!(-7));
        match doc {
            Document::Number(aws_smithy_types::Number::NegInt(-7)) => {}
            other => panic!("expected NegInt(-7), got {other:?}"),
        }
    }
}

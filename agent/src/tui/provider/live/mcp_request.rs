//! Server-initiated MCP requests (Phase 4): sampling and elicitation relayed
//! from the rmcp handler, answered here.
//!
//! Sampling rides the permission engine (`server.sampling` tool key, scopes
//! built from the message texts) and, when allowed, one bare model call — no
//! tools, the server's `maxTokens`. Elicitation maps its schema onto the
//! question form (text and single-select enum fields only).

use std::sync::Arc;

use rig_core::completion::{CompletionModel, CompletionResponse, message::AssistantContent};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use crate::history;
use crate::mcp::request::{SamplingParams, SamplingResult};
use crate::mcp::{ElicitOutcome, McpServerRequest};
use crate::permissions::{
    ASK_TIMEOUT, PermissionAnswer, PermissionCheck, PermissionManager, ToolKey,
};
use crate::run;
use crate::tools::{QuestionOption, QuestionSpec};

use super::SessionState;
use super::approval::record_answer;
use super::question::QuestionAsker;
use super::turn::resolve_model;
use crate::tui::provider::cards;
use crate::tui::provider::{AgentEvent, Status, Tone, ToolCallData};

/// Everything a server request needs; cloned out of [`super::LoopCtx`] so
/// each request runs on its own task without borrowing the command loop.
pub(super) struct ServerRequestCtx {
    pub(super) state: Arc<Mutex<SessionState>>,
    pub(super) evt_tx: mpsc::UnboundedSender<AgentEvent>,
    pub(super) permissions: Arc<PermissionManager>,
    pub(super) config: Arc<Config>,
    pub(super) selection: super::Selection,
}

/// Answer one server request off the command loop, so a slow model call or a
/// parked form never blocks user commands. The reply oneshot is fired here;
/// dropping it (loop shutdown) reads as a denial on the handler side.
pub(super) fn spawn_server_request(ctx: ServerRequestCtx, request: McpServerRequest) {
    tokio::spawn(async move {
        match request {
            McpServerRequest::Sampling {
                server,
                request,
                reply,
            } => {
                let _ = reply.send(run_sampling(&ctx, &server, request).await);
            }
            McpServerRequest::Elicitate {
                server: _,
                message,
                schema,
                reply,
            } => {
                let _ = reply.send(run_elicitation(&ctx, &message, &schema).await);
            }
        }
    });
}

/// Permission scopes for a sampling request: the message texts, so a rule
/// (or a deny) can be about what the server actually wants the model to see.
#[allow(deprecated)] // sampling types are deprecated upstream; still answered
fn sampling_scopes(request: &SamplingParams) -> Vec<String> {
    request
        .messages
        .iter()
        .map(|message| sampling_text(&message.content))
        .collect()
}

#[allow(deprecated)]
fn sampling_text(
    content: &rmcp::model::SamplingContent<rmcp::model::SamplingMessageContentBlock>,
) -> String {
    use rmcp::model::SamplingContent as C;
    use rmcp::model::SamplingMessageContentBlock as B;
    let blocks: Vec<_> = match content {
        C::Single(block) => vec![block],
        C::Multiple(blocks) => blocks.iter().collect(),
    };
    blocks
        .into_iter()
        .map(|block| match block {
            B::Text(text) => text.text.clone(),
            B::Image(_) => "[image omitted]".into(),
            B::Audio(_) => "[audio omitted]".into(),
            B::ToolUse(_) | B::ToolResult(_) => "[tool content omitted]".into(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Permission gate for a server request: pre-decided by rules, or asked
/// through the approval overlay (same parking pattern as the tool gate).
async fn check_permission(
    ctx: &ServerRequestCtx,
    tool: &ToolKey,
    scopes: &[String],
    what: &str,
) -> Result<(), String> {
    match ctx.permissions.check_multi(tool, scopes, false) {
        PermissionCheck::Allowed => Ok(()),
        PermissionCheck::Denied => Err(format!("{what} denied by permission rules")),
        PermissionCheck::NeedsPrompt { .. } => ask_permission(ctx, tool, scopes, what).await,
    }
}

/// Park on the approval overlay until the user decides (or the ask timeout
/// denies). Mirrors the tool gate: `PermissionRequest` out, oneshot back.
async fn ask_permission(
    ctx: &ServerRequestCtx,
    tool: &ToolKey,
    scopes: &[String],
    what: &str,
) -> Result<(), String> {
    let id = crate::id::CraftId::generate().to_string();
    let _ = ctx.evt_tx.send(AgentEvent::ToolCall(ToolCallData {
        id: id.clone(),
        kind: cards::tool_head(
            &tool.to_string(),
            &serde_json::json!({ "scopes": scopes.len() }),
        ),
        lines: Vec::new(),
        awaiting_approval: true,
        image: None,
    }));
    let _ = ctx
        .evt_tx
        .send(AgentEvent::StatusChanged(Status::WaitingApproval));
    // Register before emitting: an answer racing in on event receipt must
    // find the oneshot already parked.
    let (decision_tx, mut decision_rx) = tokio::sync::oneshot::channel();
    ctx.state.lock().await.pending_approval = Some((id.clone(), decision_tx));
    let _ = ctx.evt_tx.send(AgentEvent::PermissionRequest {
        id: id.clone(),
        tool: tool.to_string(),
        scopes: scopes.to_vec(),
        files: Vec::new(),
        commands: scopes.to_vec(),
    });
    let answer = tokio::time::timeout(ASK_TIMEOUT, &mut decision_rx)
        .await
        .unwrap_or(Ok(PermissionAnswer::Deny))
        .unwrap_or(PermissionAnswer::Deny);
    ctx.state.lock().await.pending_approval = None;
    let _ = ctx.evt_tx.send(AgentEvent::PermissionResolved { id });
    let _ = ctx.evt_tx.send(AgentEvent::StatusChanged(Status::Running));
    if record_answer(&ctx.permissions, tool, scopes, &answer) {
        Ok(())
    } else {
        Err(format!("{what} denied by user"))
    }
}

/// One sampling request: permission gate, then one bare model call with the
/// server's parameters mapped onto the current selection.
#[allow(deprecated)] // sampling types are deprecated upstream; still answered
async fn run_sampling(
    ctx: &ServerRequestCtx,
    server: &str,
    request: SamplingParams,
) -> Result<SamplingResult, String> {
    let tool = ToolKey::McpTool {
        server: Arc::from(server),
        tool: Arc::from("sampling"),
    };
    let scopes = sampling_scopes(&request);
    check_permission(ctx, &tool, &scopes, "sampling").await?;
    let model = resolve_model(&ctx.config, &ctx.selection)
        .await
        .map_err(|message| format!("sampling unavailable: {message}"))?;
    let _ = ctx.evt_tx.send(AgentEvent::Notice {
        tone: Tone::Info,
        text: format!(
            "mcp {server}: sampling ({} messages)",
            request.messages.len()
        ),
    });
    sampling_completion(&model, &request, &ctx.selection.model).await
}

/// The bare completion behind sampling: the server's messages mapped to the
/// agent's message type, its system prompt and `maxTokens` honored, no tools.
/// Separated from [`run_sampling`] so tests can drive it with a mock model.
#[allow(deprecated)]
pub(super) async fn sampling_completion<M: CompletionModel>(
    model: &M,
    request: &SamplingParams,
    model_label: &str,
) -> Result<SamplingResult, String> {
    #[allow(deprecated)]
    use rmcp::model::{Role, SamplingMessage};
    let messages: Vec<history::Message> = request
        .messages
        .iter()
        .map(|message: &SamplingMessage| {
            let text = sampling_text(&message.content);
            match message.role {
                Role::Assistant => history::Message::assistant(text),
                Role::User => history::Message::user(text),
            }
        })
        .collect();
    let rig_request = crate::edge::to_request(
        &messages,
        &[],
        request.system_prompt.as_deref(),
        request.temperature.map(f64::from),
        Some(u64::from(request.max_tokens)),
    );
    let response: CompletionResponse = model
        .completion(rig_request)
        .await
        .map_err(|e| format!("sampling model call failed: {e}"))?;
    let text = response
        .choice
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    #[allow(deprecated)]
    let message = rmcp::model::SamplingMessage::assistant_text(text);
    #[allow(deprecated)]
    Ok(SamplingResult::new(message, model_label.to_owned())
        .with_stop_reason(rmcp::model::CreateMessageResult::STOP_REASON_END_TURN))
}

/// One elicitation request: map the schema to question-form fields, ask,
/// and fold the answers back into the MCP result object.
async fn run_elicitation(
    ctx: &ServerRequestCtx,
    message: &str,
    schema: &rmcp::model::ElicitationSchema,
) -> Result<ElicitOutcome, String> {
    let fields = elicit_fields(schema)?;
    // The server's message frames the form: prepend it to each question so
    // the user sees what the server wants, not just field names.
    let specs: Vec<QuestionSpec> = fields
        .iter()
        .map(|(_, spec)| QuestionSpec {
            question: format!("{message}\n\n{}", spec.question),
            ..spec.clone()
        })
        .collect();
    // A fresh cancel channel: these asks are not part of a turn, so nothing
    // but the ask timeout or the user can end them.
    let (_flag, cancel) = run::cancel_channel();
    let asker = QuestionAsker::new(ctx.state.clone(), ctx.evt_tx.clone(), cancel);
    let answer = crate::tools::AskQuestions::ask(&asker, specs).await;
    if answer.dismissed {
        return Ok(ElicitOutcome::Decline);
    }
    let mut content = serde_json::Map::new();
    for ((name, _), values) in fields.iter().zip(answer.answers) {
        if let Some(value) = values.first() {
            content.insert(name.clone(), Value::String(value.clone()));
        }
    }
    Ok(ElicitOutcome::Accept(Value::Object(content)))
}

/// Map an elicitation schema onto question-form fields. Only text and
/// single-select enum properties are supported; anything richer answers
/// `unsupported elicit schema` so the server can degrade.
fn elicit_fields(
    schema: &rmcp::model::ElicitationSchema,
) -> Result<Vec<(String, QuestionSpec)>, String> {
    use rmcp::model::{EnumSchema, PrimitiveSchemaDefinition, SingleSelectEnumSchema};
    let order = schema
        .property_order
        .clone()
        .unwrap_or_else(|| schema.properties.keys().cloned().collect());
    let mut fields = Vec::new();
    for name in order {
        let Some(definition) = schema.properties.get(&name) else {
            continue;
        };
        let (options, title, description) = match definition {
            PrimitiveSchemaDefinition::String(string) => {
                (Vec::new(), string.title.clone(), string.description.clone())
            }
            PrimitiveSchemaDefinition::Enum(EnumSchema::Single(
                SingleSelectEnumSchema::Untitled(untitled),
            )) => (
                untitled.enum_.clone(),
                untitled.title.clone(),
                untitled.description.clone(),
            ),
            PrimitiveSchemaDefinition::Enum(EnumSchema::Single(
                SingleSelectEnumSchema::Titled(titled),
            )) => (
                titled
                    .one_of
                    .iter()
                    .map(|item| item.const_.clone())
                    .collect(),
                titled.title.clone(),
                titled.description.clone(),
            ),
            PrimitiveSchemaDefinition::Enum(EnumSchema::Legacy(legacy)) => (
                legacy.enum_.clone(),
                legacy.title.clone(),
                legacy.description.clone(),
            ),
            // Multi-select enums and non-string primitives are not things
            // the question form can faithfully render.
            _ => return Err("unsupported elicit schema".into()),
        };
        let question = description
            .filter(|text| !text.trim().is_empty())
            .map(|text| text.into_owned())
            .unwrap_or_else(|| name.clone());
        fields.push((
            name.clone(),
            QuestionSpec {
                question,
                header: title.map(|t| t.into_owned()).or_else(|| Some(name.clone())),
                options: options
                    .into_iter()
                    .map(|label| QuestionOption {
                        label,
                        description: None,
                    })
                    .collect(),
                multi_select: false,
            },
        ));
    }
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(properties: serde_json::Value) -> rmcp::model::ElicitationSchema {
        serde_json::from_value(properties).expect("valid elicitation schema")
    }

    fn ctx() -> ServerRequestCtx {
        ServerRequestCtx {
            state: Arc::new(Mutex::new(SessionState::default())),
            evt_tx: mpsc::unbounded_channel().0,
            permissions: Arc::new(PermissionManager::new(
                crate::permissions::PermissionsConfig::default(),
                std::env::temp_dir(),
            )),
            config: Arc::new(Config::default()),
            selection: super::super::Selection {
                provider: "p".into(),
                model: "m".into(),
                context_length: None,
            },
        }
    }

    #[allow(deprecated)]
    fn sampling_request(texts: &[&str]) -> SamplingParams {
        use rmcp::model::SamplingMessage;
        SamplingParams::new(
            texts
                .iter()
                .map(|t| SamplingMessage::user_text(*t))
                .collect(),
            128,
        )
    }

    #[test]
    fn elicit_text_and_enum_fields_map_to_questions() {
        let schema = schema(serde_json::json!({
            "type": "object",
            "properties": {
                "note": { "type": "string", "description": "A note" },
                "flavor": { "type": "string", "enum": ["a", "b"] }
            }
        }));
        let fields = elicit_fields(&schema).expect("supported");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "note");
        assert_eq!(fields[0].1.question, "A note");
        assert!(fields[0].1.options.is_empty(), "text field is free input");
        assert_eq!(fields[1].1.options.len(), 2);
        assert_eq!(fields[1].1.options[0].label, "a");
    }

    #[test]
    fn elicit_non_string_properties_are_rejected() {
        // Elicitation schemas are flat objects of primitives; anything the
        // form cannot render — numbers, booleans, multi-select — is refused
        // rather than silently mistranslated.
        let schema = schema(serde_json::json!({
            "type": "object",
            "properties": { "age": { "type": "integer" } }
        }));
        assert_eq!(
            elicit_fields(&schema).unwrap_err(),
            "unsupported elicit schema"
        );
    }

    #[test]
    fn sampling_scopes_carry_the_message_texts() {
        let scopes = sampling_scopes(&sampling_request(&["hello", "world"]));
        assert_eq!(scopes, vec!["hello".to_string(), "world".to_string()]);
    }

    /// Deny at the overlay: the pending approval is answered with Deny and
    /// the relay surfaces the MCP error message.
    #[tokio::test]
    async fn sampling_denied_at_the_overlay_returns_the_mcp_error() {
        let ctx = ctx();
        let state = ctx.state.clone();
        let tool = ToolKey::McpTool {
            server: Arc::from("github"),
            tool: Arc::from("sampling"),
        };
        let pending = tokio::spawn(async move {
            ask_permission(&ctx, &tool, &["secret".to_string()], "sampling").await
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while state.lock().await.pending_approval.is_none() {
            assert!(std::time::Instant::now() < deadline, "never parked");
            tokio::task::yield_now().await;
        }
        super::super::approval::decide(&state, park_id(&state).await, PermissionAnswer::Deny).await;
        let err = pending.await.unwrap().unwrap_err();
        assert_eq!(err, "sampling denied by user");
    }

    async fn park_id(state: &Arc<Mutex<SessionState>>) -> String {
        state
            .lock()
            .await
            .pending_approval
            .as_ref()
            .map(|(id, _)| id.clone())
            .expect("parked")
    }

    /// A persisted deny rule short-circuits before the overlay.
    #[tokio::test]
    async fn sampling_denied_by_rule_never_parks() {
        let ctx = ctx();
        ctx.permissions
            .add_session_rule(crate::permissions::PermissionRule {
                tool: ToolKey::McpTool {
                    server: Arc::from("github"),
                    tool: Arc::from("sampling"),
                },
                scope: Some("nope".to_string()),
                effect: crate::permissions::Effect::Deny,
            });
        let tool = ToolKey::McpTool {
            server: Arc::from("github"),
            tool: Arc::from("sampling"),
        };
        let outcome = check_permission(&ctx, &tool, &["nope".to_string()], "sampling").await;
        assert_eq!(outcome.unwrap_err(), "sampling denied by permission rules");
        assert!(ctx.state.lock().await.pending_approval.is_none());
    }

    /// Allowed sampling runs the bare model call and maps the reply back to
    /// the MCP result shape.
    #[tokio::test]
    #[allow(deprecated)] // asserting on the deprecated sampling result fields
    async fn sampling_completion_maps_the_model_reply() {
        use rig_core::test_utils::MockCompletionModel;
        let model = MockCompletionModel::text("sampled!");
        let result = sampling_completion(&model, &sampling_request(&["hi"]), "test-model")
            .await
            .unwrap();
        assert_eq!(result.message.role, rmcp::model::Role::Assistant);
        assert_eq!(result.model, "test-model");
        assert_eq!(result.stop_reason.as_deref(), Some("endTurn"));
        assert_eq!(sampling_text(&result.message.content), "sampled!");
    }

    /// The elicitation form round-trip: request parks, the answer folds into
    /// the MCP result object; dismissal declines.
    #[tokio::test]
    async fn elicitation_round_trips_through_the_question_form() {
        let ctx = ctx();
        let state = ctx.state.clone();
        let schema = schema(serde_json::json!({
            "type": "object",
            "properties": {
                "note": { "type": "string" },
                "flavor": { "type": "string", "enum": ["a", "b"] }
            }
        }));
        let ask_ctx = ctx;
        let pending = tokio::spawn(async move { run_elicitation(&ask_ctx, "pick", &schema).await });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while state.lock().await.pending_question.is_none() {
            assert!(std::time::Instant::now() < deadline, "form never parked");
            tokio::task::yield_now().await;
        }
        let id = state
            .lock()
            .await
            .pending_question
            .as_ref()
            .map(|(id, _)| id.clone())
            .expect("parked");
        super::super::question::answer_question(
            &state,
            id,
            crate::tools::QuestionAnswer {
                dismissed: false,
                answers: vec![vec!["typed".into()], vec!["b".into()]],
            },
        )
        .await;
        match pending.await.unwrap().unwrap() {
            ElicitOutcome::Accept(content) => {
                assert_eq!(
                    content,
                    serde_json::json!({ "note": "typed", "flavor": "b" })
                );
            }
            other => panic!("expected accept, got {other:?}"),
        }
    }
}

//! Server-initiated MCP requests (Phase 4): elicitation relayed from the
//! rmcp handler, answered here.
//!
//! Elicitation maps its schema onto the question form (text and
//! single-select enum fields only). Sampling is deprecated by SEP-2577 and
//! no longer supported.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::mcp::{ElicitOutcome, McpServerRequest};
use crate::run;
use crate::tools::{QuestionOption, QuestionSpec};

use super::SessionState;
use super::question::QuestionAsker;
use crate::tui::provider::AgentEvent;

/// Everything a server request needs; cloned out of [`super::LoopCtx`] so
/// each request runs on its own task without borrowing the command loop.
pub(super) struct ServerRequestCtx {
    pub(super) state: Arc<Mutex<SessionState>>,
    pub(super) evt_tx: mpsc::UnboundedSender<AgentEvent>,
}

/// Answer one server request off the command loop, so a parked form never
/// blocks user commands. The reply oneshot is fired here; dropping it (loop
/// shutdown) reads as a denial on the handler side.
pub(super) fn spawn_server_request(ctx: ServerRequestCtx, request: McpServerRequest) {
    tokio::spawn(async move {
        match request {
            McpServerRequest::Elicitate {
                message,
                schema,
                reply,
                ..
            } => {
                let _ = reply.send(run_elicitation(&ctx, &message, &schema).await);
            }
        }
    });
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
        }
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

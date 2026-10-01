// ---------------------------------------------------------------------------
// G.5 Elicitation: the `question` tool over `elicitation/create`
// ---------------------------------------------------------------------------

use agent_client_protocol::{
    Client as AcpClient, ConnectionTo, Error,
    schema::v1::{
        ClientCapabilities, CreateElicitationRequest, ElicitationContentValue, ElicitationFormMode,
        ElicitationPropertySchema, ElicitationSchema, ElicitationScope, ElicitationSessionScope,
        EnumOption, MultiSelectPropertySchema, SessionId, StringPropertySchema, ToolCallId,
    },
};

use crate::{
    permissions::ASK_TIMEOUT,
    run,
    tools::{AskQuestions, QuestionAnswer, QuestionOption, QuestionSpec},
};

pub(super) fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

fn enum_options(options: &[QuestionOption]) -> Vec<EnumOption> {
    options
        .iter()
        .map(|opt| {
            let title = match opt.description.as_deref() {
                Some(description) if !description.is_empty() => {
                    format!("{} - {}", opt.label, description)
                }
                _ => opt.label.clone(),
            };
            EnumOption::new(opt.label.clone(), title)
        })
        .collect()
}

fn property(q: &QuestionSpec) -> ElicitationPropertySchema {
    let title = q.question.clone();
    if q.options.is_empty() {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title))
    } else if q.multi_select {
        ElicitationPropertySchema::Array(
            MultiSelectPropertySchema::titled(enum_options(&q.options)).title(title),
        )
    } else {
        ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .one_of(enum_options(&q.options)),
        )
    }
}

/// Property keys are positional (`q1`, `q2`, ...) so answers map back to
/// questions even when headers repeat or are missing.
fn question_key(index: usize) -> String {
    format!("q{}", index + 1)
}

pub(super) fn form_request(
    session_id: &SessionId,
    tool_call_id: Option<String>,
    questions: &[QuestionSpec],
) -> Result<CreateElicitationRequest, String> {
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }

    let mut schema = ElicitationSchema::new();
    schema.properties = questions
        .iter()
        .enumerate()
        .map(|(i, q)| (question_key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(session_id.0.to_string())
        .tool_call_id(tool_call_id.map(ToolCallId::from));
    let message = match questions {
        [only] => only.question.clone(),
        many => format!("{} questions", many.len()),
    };
    Ok(CreateElicitationRequest::new(
        ElicitationFormMode::new(ElicitationScope::Session(scope), schema),
        message,
    ))
}

fn content_labels(value: &ElicitationContentValue) -> Vec<String> {
    match value {
        ElicitationContentValue::String(s) if !s.is_empty() => vec![s.clone()],
        ElicitationContentValue::StringArray(items) if !items.is_empty() => items.clone(),
        ElicitationContentValue::Integer(n) => vec![n.to_string()],
        ElicitationContentValue::Number(n) => vec![n.to_string()],
        ElicitationContentValue::Boolean(b) => vec![b.to_string()],
        _ => Vec::new(),
    }
}

/// `elicitation/create` with an untyped response: the reference parses the
/// raw JSON-RPC result, and the typed `CreateElicitationResponse` cannot
/// represent nulled-out content values ("one bad value costs one answer, not
/// the form"), so this wrapper keeps the wire shape but returns raw JSON.
const ELICITATION_METHOD: &str = "elicitation/create";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
struct RawElicitationRequest(CreateElicitationRequest);

impl agent_client_protocol::JsonRpcMessage for RawElicitationRequest {
    fn matches_method(method: &str) -> bool {
        method == ELICITATION_METHOD
    }

    fn method(&self) -> &str {
        ELICITATION_METHOD
    }

    fn to_untyped_message(&self) -> Result<agent_client_protocol::UntypedMessage, Error> {
        agent_client_protocol::UntypedMessage::new(ELICITATION_METHOD, self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if !Self::matches_method(method) {
            return Err(Error::method_not_found());
        }
        let value = serde_json::to_value(params).map_err(|e| {
            let mut error = Error::invalid_params();
            error.message = e.to_string();
            error
        })?;
        serde_json::from_value(value)
            .map(RawElicitationRequest)
            .map_err(|e| {
                let mut error = Error::invalid_params();
                error.message = e.to_string();
                error
            })
    }
}

impl agent_client_protocol::JsonRpcRequest for RawElicitationRequest {
    type Response = serde_json::Value;
}

/// Turns the client's `elicitation/create` result into the encoded answer
/// the `question` tool already understands. Ported from the reference
/// `elicitation.rs`: values parse per key — nothing in the schema is
/// `required`, so clients may null out skipped fields, and one unreadable
/// value should cost one answer, not the whole form.
pub(super) fn answer_from_response(raw: &serde_json::Value) -> QuestionAnswer {
    let dismissed = QuestionAnswer {
        dismissed: true,
        answers: vec![],
    };
    if raw["action"] != "accept" {
        return dismissed;
    }
    let Some(content) = raw["content"].as_object() else {
        return dismissed;
    };

    let mut answers: Vec<Vec<String>> = Vec::new();
    for (k, v) in content {
        let Ok(index) = k.trim_start_matches('q').parse::<usize>() else {
            continue;
        };
        // One unreadable value costs one answer, not the form: the key is
        // dropped, exactly like the reference's filter_map.
        let Ok(value) = serde_json::from_value::<ElicitationContentValue>(v.clone()) else {
            continue;
        };
        let labels = content_labels(&value);
        if answers.len() < index {
            answers.resize(index, Vec::new());
        }
        answers[index - 1] = labels;
    }
    QuestionAnswer {
        dismissed: false,
        answers,
    }
}

/// Ask the client through `elicitation/create`; clients without form
/// capability get the headless dismissed answer.
pub(super) struct ElicitationAsker {
    pub(super) connection: ConnectionTo<AcpClient>,
    pub(super) session_id: SessionId,
    pub(super) cancel: run::CancelToken,
    pub(super) caps: ClientCapabilities,
}

impl AskQuestions for ElicitationAsker {
    fn ask(&self, questions: Vec<QuestionSpec>) -> run::BoxFuture<QuestionAnswer> {
        let asker = ElicitationAsker {
            connection: self.connection.clone(),
            session_id: self.session_id.clone(),
            cancel: self.cancel.clone(),
            caps: self.caps.clone(),
        };
        Box::pin(async move { asker.ask(questions).await })
    }
}

impl ElicitationAsker {
    async fn ask(self, questions: Vec<QuestionSpec>) -> QuestionAnswer {
        let dismissed = QuestionAnswer {
            dismissed: true,
            answers: vec![],
        };
        if !supports_form(&self.caps) {
            return dismissed;
        }
        let Ok(request) = form_request(&self.session_id, None, &questions) else {
            return dismissed;
        };
        let sent = self.connection.send_request(RawElicitationRequest(request));
        let mut cancel_rx = self.cancel.subscribe();
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                let _ = changed;
                dismissed
            }
            response = tokio::time::timeout(ASK_TIMEOUT, sent.block_task()) => match response {
                Ok(Ok(response)) => answer_from_response(&response),
                _ => dismissed,
            },
        }
    }
}

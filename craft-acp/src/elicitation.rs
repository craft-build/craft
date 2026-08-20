//! Maps the `question` tool onto ACP form elicitation (`elicitation/create`).
//! The tool keeps its advertised schema; only the answer round-trip is
//! rerouted here, so Zed and friends render native forms instead of a custom
//! request they cannot answer.

use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationScope, ElicitationSessionScope, EnumOption, MultiSelectPropertySchema,
    StringPropertySchema, ToolCallId,
};
use craft_agent::tools::question::QuestionAnswer;
use craft_agent::types::QuestionSpec;

pub fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

fn enum_options(options: &[craft_agent::types::QuestionOption]) -> Vec<EnumOption> {
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
fn key(index: usize) -> String {
    format!("q{}", index + 1)
}

pub fn form_request(
    session_id: &str,
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
        .map(|(i, q)| (key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(session_id.to_owned())
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

fn labels(value: Option<&ElicitationContentValue>) -> Vec<String> {
    match value {
        Some(ElicitationContentValue::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(ElicitationContentValue::StringArray(items)) if !items.is_empty() => items.clone(),
        Some(ElicitationContentValue::Integer(n)) => vec![n.to_string()],
        Some(ElicitationContentValue::Number(n)) => vec![n.to_string()],
        Some(ElicitationContentValue::Boolean(b)) => vec![b.to_string()],
        _ => Vec::new(),
    }
}

/// Turns the client's `elicitation/create` result into the encoded
/// [`QuestionAnswer`] the agent's question dispatch already understands, so
/// the model sees the same markdown regardless of frontend.
pub fn answer_from_response(raw_result: &str) -> QuestionAnswer {
    let dismissed = QuestionAnswer {
        dismissed: true,
        answers: vec![],
    };
    let Ok(response) = serde_json::from_str::<CreateElicitationResponse>(raw_result) else {
        return dismissed;
    };
    let ElicitationAction::Accept(accept) = response.action else {
        return dismissed;
    };
    let Some(content) = accept.content else {
        return dismissed;
    };

    let mut answers: Vec<Vec<String>> = Vec::new();
    for (k, v) in &content {
        let Ok(index) = k.trim_start_matches('q').parse::<usize>() else {
            continue;
        };
        let labels = labels(Some(v));
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

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::SessionId;
    use test_case::test_case;

    use super::*;

    fn questions() -> Vec<QuestionSpec> {
        craft_agent::tools::question::parse_questions(&serde_json::json!({
            "questions": [
                {
                    "question": "Pick a framework",
                    "header": "Framework",
                    "options": [
                        { "label": "axum", "description": "tokio based" },
                        { "label": "actix" }
                    ]
                },
                {
                    "question": "Which features?",
                    "header": "Features",
                    "multiSelect": true,
                    "options": [{ "label": "auth" }, { "label": "uploads" }]
                },
                { "question": "Anything else?" }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn form_request_maps_questions_to_schema() {
        let req = form_request("sess_1", Some("tool_1".to_owned()), &questions()).unwrap();
        assert_eq!(req.message, "3 questions");

        let ElicitationScope::Session(scope) = req.scope() else {
            panic!("expected session scope");
        };
        assert_eq!(scope.session_id.0.as_ref(), "sess_1");
        assert_eq!(
            scope.tool_call_id.as_ref().map(|t| t.0.as_ref()),
            Some("tool_1")
        );

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["mode"], "form");
        let props = &json["requestedSchema"]["properties"];
        assert_eq!(props["q1"]["type"], "string");
        assert_eq!(props["q1"]["oneOf"][0]["const"], "axum");
        assert_eq!(props["q2"]["type"], "array");
        assert_eq!(props["q3"]["type"], "string");
        assert!(props["q3"].get("oneOf").is_none());
    }

    #[test]
    fn single_question_is_the_message() {
        let qs = vec![QuestionSpec {
            question: "Proceed?".into(),
            header: None,
            options: vec![],
            multi_select: false,
        }];
        let req = form_request("sess_1", None, &qs).unwrap();
        assert_eq!(req.message, "Proceed?");
    }

    #[test]
    fn form_request_rejects_empty_questions() {
        assert!(form_request("sess_1", None, &[]).is_err());
    }

    #[test]
    fn accept_response_maps_answers_by_position() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q1": "axum", "q2": ["auth", "uploads"] }
        })
        .to_string();
        let answer = answer_from_response(&raw);
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["axum"], vec!["auth", "uploads"]]);
    }

    #[test]
    fn missing_answer_becomes_empty_labels() {
        let raw = serde_json::json!({
            "action": "accept",
            "content": { "q2": ["auth"] }
        })
        .to_string();
        let answer = answer_from_response(&raw);
        assert_eq!(answer.answers, vec![vec![], vec!["auth"]]);
    }

    #[test_case(r#"{"action":"decline"}"# ; "decline")]
    #[test_case(r#"{"action":"cancel"}"# ; "cancel")]
    #[test_case("not json" ; "unparsable")]
    #[test_case("null" ; "jsonrpc_error_forwarded_as_null")]
    fn non_accept_is_dismissed(raw: &str) {
        assert!(answer_from_response(raw).dismissed);
    }

    #[test]
    fn supports_form_requires_form_capability() {
        assert!(!supports_form(&ClientCapabilities::default()));
        let caps: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "form": {} }
        }))
        .unwrap();
        assert!(supports_form(&caps));
        let url_only: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "url": {} }
        }))
        .unwrap();
        assert!(!supports_form(&url_only));
    }

    #[test]
    fn scope_serializes_session_id() {
        let req = form_request("sess_1", None, &questions()).unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["sessionId"], SessionId::from("sess_1").0.as_ref());
    }
}

use super::elicitation::*;
use super::permissions::*;
use super::turn::*;
use super::*;
use crate::history;
use crate::permissions::{PermissionAnswer, PermissionCheck, ToolKey, scope_for_call};
use crate::run;
use crate::tools::{QuestionOption, QuestionSpec};
use agent_client_protocol::schema::v1::{
    Content, RequestPermissionOutcome, ResourceLink, TextContent, ToolCallContent, ToolKind,
};

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn link(name: &str) -> ContentBlock {
    ContentBlock::ResourceLink(ResourceLink::new(name, format!("file:///tmp/{name}")))
}

#[test]
fn prompt_text_joins_blocks_and_lists_context_files() {
    let prompt = vec![text_block("Please fix it"), link("src/main.rs")];
    let text = prompt_text(&prompt).unwrap();
    assert!(text.starts_with("Please fix it\n\nContext files:\n- src/main.rs\n"));
}

#[test]
fn prompt_text_rejects_unsupported_blocks() {
    assert!(prompt_text(&[link("a"), text_block("x")]).is_ok());
    let resource = serde_json::from_value::<ContentBlock>(serde_json::json!({
        "type": "resource",
        "resource": { "uri": "file:///tmp/x", "mimeType": "text/plain", "text": "hi" }
    }))
    .unwrap();
    assert!(prompt_text(&[resource]).is_err());
}

#[test]
fn config_options_expose_provider_model_and_thinking() {
    let session = test_session();
    let options =
        session.config_options(&Config::default(), &["openai".into(), "llamafile".into()]);
    assert_eq!(options.len(), 3);
    assert_eq!(options[0].id.0.as_ref(), "provider");
    assert_eq!(options[1].id.0.as_ref(), "model");
    assert_eq!(options[2].id.0.as_ref(), "thinking");
}

fn mock_model() -> crate::provider::ModelHandle {
    let provider = crate::testing::ScriptedProvider::new(Vec::new());
    crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
        &provider,
        crate::testing::model_info("mock-model", 200_000, 8_192, false, false),
    ))
}

fn test_session() -> Session {
    let (sink, events) = crate::harness::EventSink::new();
    let history = animist::history::History::new();
    let conversation = crate::harness::ConversationBuilder::new(
        &Config::default(),
        &std::env::temp_dir().display().to_string(),
        "",
        mock_model(),
        Workspace::new(std::env::temp_dir())
            .expect("workspace")
            .registry(&history)
            .unwrap(),
        sink,
    )
    .history(history)
    .build()
    .expect("session conversation builds");
    Session {
        conversation,
        events,
        cancel_cell: Arc::new(arc_swap::ArcSwap::from_pointee(
            animist::agent::Cancellation::default(),
        )),
        persister: None,
        provider_name: "openai".into(),
        models: Vec::new(),
        model: "gpt-x".into(),
        context_length: Some(128_000),
        thinking: Default::default(),
        turn: None,
    }
}

#[test]
fn thinking_options_include_current_budget() {
    let info = efforts_info(&["low", "high"]);
    let option = thinking_option(crate::thinking::ThinkingConfig::Budget(4096), Some(&info));
    let json = serde_json::to_value(option).unwrap();
    let current = crate::thinking::ThinkingConfig::Budget(4096).to_string();
    assert_eq!(json["currentValue"], current);
    let values: Vec<_> = json["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, ["off", "adaptive", "low", "high", &current]);
}

/// A `ModelInfo` reporting discrete efforts, as a provider listing or the
/// models.dev catalog would.
fn efforts_info(efforts: &[&str]) -> animist::provider::ModelInfo {
    let parsed: Vec<animist::provider::Effort> = efforts
        .iter()
        .map(|effort| match *effort {
            "low" => animist::provider::Effort::Low,
            "high" => animist::provider::Effort::High,
            other => panic!("unsupported test effort {other}"),
        })
        .collect();
    let mut info = crate::testing::model_info_with("gpt-x", None, None);
    info.features.thinking = animist::provider::Thinking::builder()
        .supported(true)
        .required(false)
        .efforts(parsed)
        .build();
    info
}

#[test]
fn thinking_options_filter_levels_without_restoring_unsupported_current_effort() {
    let info = efforts_info(&["low", "high"]);
    let mut session = test_session();
    session.models = vec![info];
    session.thinking = crate::thinking::ThinkingConfig::Effort(crate::thinking::Effort::Max);
    let options = session.config_options(&Config::default(), &["openai".into()]);
    let json = serde_json::to_value(&options[2]).unwrap();
    assert_eq!(json["currentValue"], "high");
    assert_eq!(
        session.thinking,
        crate::thinking::ThinkingConfig::Effort(crate::thinking::Effort::Max)
    );
    let values: Vec<_> = json["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, ["off", "adaptive", "low", "high"]);
}

#[test]
fn second_prompt_while_turn_in_flight_is_rejected() {
    let mut session = test_session();
    session.begin_turn(1).unwrap();
    // A second prompt is refused rather than queued, and must not
    // overtake the in-flight turn's slot.
    let error = session.begin_turn(2).unwrap_err();
    assert!(error.contains("in flight"), "unexpected: {error}");
    assert_eq!(session.turn, Some(1));
    // Once the in-flight turn releases the slot, the next prompt is
    // accepted again.
    session.turn = None;
    session.begin_turn(2).unwrap();
    assert_eq!(session.turn, Some(2));
}

#[test]
fn in_flight_rejection_maps_to_a_json_rpc_error() {
    let error = invalid_request("a prompt is already in flight for this session");
    assert_eq!(error.code, Error::invalid_request().code);
    assert!(error.message.contains("in flight"));
}

#[tokio::test]
async fn turn_slots_are_exclusive_and_released_by_clear_turn() {
    // History commits live in the conversation now; what the session
    // still arbitrates is the single in-flight turn slot.
    let session_id = SessionId::new("s1");
    let sessions = Mutex::new(BTreeMap::from([("s1".to_string(), test_session())]));
    sessions
        .lock()
        .await
        .get_mut("s1")
        .unwrap()
        .begin_turn(1)
        .unwrap();
    let error = sessions
        .lock()
        .await
        .get_mut("s1")
        .unwrap()
        .begin_turn(2)
        .unwrap_err();
    assert!(error.contains("in flight"), "unexpected: {error}");
    // The settled turn releases the slot; the next prompt is accepted.
    clear_turn(&sessions, &session_id, 1).await;
    sessions
        .lock()
        .await
        .get_mut("s1")
        .unwrap()
        .begin_turn(2)
        .unwrap();
    assert_eq!(sessions.lock().await.get("s1").unwrap().turn, Some(2));
}

#[test]
fn tool_kinds_follow_the_registered_tools() {
    assert_eq!(tool_kind("read"), ToolKind::Read);
    assert_eq!(tool_kind("grep"), ToolKind::Search);
    assert_eq!(tool_kind("edit"), ToolKind::Edit);
    assert_eq!(tool_kind("delete"), ToolKind::Delete);
    assert_eq!(tool_kind("other"), ToolKind::Other);
}

#[tokio::test]
async fn registered_tools_route_task_calls_to_the_installed_launcher() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    // The launcher the session installs: a `task` call with a bad
    // subagent type reaches its spawn and fails informatively instead of
    // degrading to the no-host availability error.
    let (sink, _stream) = crate::harness::EventSink::new();
    let cwd = dir.path().display().to_string();
    let history = animist::history::History::new();
    let parent = crate::harness::ConversationBuilder::new(
        &Config::default(),
        &cwd,
        "",
        mock_model(),
        workspace.registry(&history).unwrap(),
        sink.clone(),
    )
    .history(history)
    .build()
    .expect("parent conversation builds");
    let launcher = Arc::new(crate::subagent::SubagentLauncher {
        parent,
        provider: "mock".into(),
        providers: Default::default(),
        agent: Default::default(),
        compression: Default::default(),
        base_prompt: String::new(),
        workspace: workspace.clone(),
        cancel: Arc::new(arc_swap::ArcSwap::from_pointee(
            animist::agent::Cancellation::default(),
        )),
        cancels: Arc::new(run::cancel::CancelMap::new()),
        sink,
        policy: crate::harness::PolicyLayerInputs::new(
            Arc::new(crate::permissions::PermissionManager::new(
                crate::permissions::PermissionsConfig::default(),
                std::env::temp_dir(),
            )),
            Arc::new(crate::headless::HeadlessGate),
        ),
        cwd,
    });
    let tools = workspace
        .with_subagents(launcher)
        .registry(&animist::history::History::new())
        .unwrap();
    let call = || {
        serde_json::json!({
            "description": "probe",
            "prompt": "probe",
            "subagent_type": "bogus",
        })
    };
    let output = invoke_task(&tools, call()).await;
    let text = history::ext::tool_content_text(&output.content);
    assert!(
        text.contains("unknown subagent type: bogus"),
        "the registered task tool must reach the installed launcher: {text}"
    );

    // Counterfactual (the pre-fix ACP behavior): without an installed
    // launcher the same registered call only gets the availability error.
    let bare = Workspace::new(dir.path())
        .unwrap()
        .registry(&animist::history::History::new())
        .unwrap();
    let output = invoke_task(&bare, call()).await;
    let text = history::ext::tool_content_text(&output.content);
    assert!(
        text.contains("task tool is not available in this session"),
        "a missing launcher degrades to the availability error: {text}"
    );
}

/// Invoke the registered `task` tool through the registry, routed as a
/// batch child so the registered instance (with its installed launcher)
/// serves the call.
async fn invoke_task(
    registry: &animist::agent::tool::ToolRegistry,
    input: serde_json::Value,
) -> animist::agent::tool::ToolOutput {
    let mut harness = animist::agent::testing::ToolHarness::builder()
        .tools(registry.clone())
        .build()
        .expect("harness");
    harness
        .call(
            crate::tools::Batch,
            serde_json::json!({ "tool_calls": [{"tool": "task", "parameters": input}] }),
        )
        .await
        .expect("task batch outcome")
}

#[tokio::test]
async fn streamed_filesystem_results_match_model_and_acp_text() {
    use serde_json::json;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("file.rs"),
        "skipped\r\n\r\n    let name = \"βeta\";\r\nlast",
    )
    .unwrap();
    let mut turns: Vec<crate::testing::Attempt> = Vec::new();
    for (id, name, args) in [
        (
            "1",
            "read",
            json!({"path":"file.rs", "offset":2, "limit":2}),
        ),
        ("2", "grep", json!({"pattern":"βeta"})),
        (
            "3",
            "edit",
            json!({"path":"file.rs", "old_string":"βeta", "new_string":"new"}),
        ),
        ("4", "delete", json!({"files":["file.rs"]})),
    ] {
        turns.push(crate::testing::tool_turn(vec![(
            id.to_owned(),
            name.to_owned(),
            args,
        )]));
    }
    turns.push(crate::testing::text_turn("done"));
    let provider = crate::testing::ScriptedProvider::new(Vec::new());
    for turn in turns {
        provider.push(turn);
    }
    let model = crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
        &provider,
        crate::testing::model_info("scripted", 200_000, 8_192, false, false),
    ));
    let history = animist::history::History::new();
    let tools = Workspace::new(dir.path())
        .unwrap()
        .registry(&history)
        .unwrap();
    let (sink, stream) = crate::harness::EventSink::new();
    let conversation = crate::harness::ConversationBuilder::new(
        &Config::parse("").expect("default config"),
        &dir.path().display().to_string(),
        "",
        model,
        tools,
        sink,
    )
    .build()
    .expect("conversation builds");
    let outcome = crate::harness::run_turn(
        &conversation,
        "read, search, edit, delete",
        &[],
        animist::agent::Cancellation::default(),
    )
    .await;
    assert!(matches!(outcome, crate::harness::TurnOutcome::Done { ref reply } if reply == "done"));
    let mut results = Vec::new();
    let mut stream = stream;
    // Drain through the terminal Done: every ToolFinished precedes it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Some(event) = stream.next().await {
        if let crate::harness::Event::Agent(animist::agent::AgentEvent::ToolFinished {
            ref call,
            ref output,
        }) = event
        {
            // The model-visible text and the ACP display text must match.
            let ToolCallContent::Content(Content {
                content: ContentBlock::Text(display),
                ..
            }) = tool_result_content(call, output)
            else {
                panic!("ACP must display tool text");
            };
            assert_eq!(output.content.0.len(), 1);
            let model_text = history::ext::tool_content_text(&output.content);
            assert_eq!(display.text, model_text);
            results.push((call.name.clone(), model_text));
        }
        if matches!(
            event,
            crate::harness::Event::Craft(crate::harness::CraftEvent::Done { .. })
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no terminal Done arrived"
        );
    }
    assert_eq!(results, [
        ("read".into(), "2: \n3:     let name = \"βeta\";\n\n...\n\nTruncated lines: 4-4. Use offset=4 to read further.".into()),
        ("grep".into(), "file.rs:\n  3:     let name = \"βeta\";".into()),
        ("edit".into(), "edited file.rs\n--- file.rs\n+++ file.rs\n@@ -1 +1 @@\n  skipped\r\n  \r\n-     let name = \"βeta\";\r\n+     let name = \"new\";\r\n  last".into()),
        ("delete".into(), "deleted: file.rs".into()),
    ]);
    // Verify what the next model request actually receives, not only the
    // display events: all four results must remain literal text.
    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    let model_results = requests[4]
        .messages
        .iter()
        .flat_map(history::ext::tool_results)
        .map(|block| {
            let history::ContentBlock::ToolResult { name, content, .. } = block else {
                unreachable!("tool_results yields ToolResult blocks only");
            };
            assert_eq!(content.0.len(), 1);
            (
                name.clone().unwrap_or_default(),
                history::ext::tool_content_text(content),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(model_results, results);
    assert!(!dir.path().join("file.rs").exists());
}

#[test]
fn tool_result_text_preserves_literal_json_and_errors() {
    for text in [r#"{"lines":[],"total_lines":0}"#, "file not found", ""] {
        let result = history::ext::tool_result("1", None, text, false);
        assert_eq!(tool_result_text(&result), text);
    }
}

#[test]
fn tool_result_text_pretty_prints_json_results() {
    // JSON results render to pretty text inside a Text block at conversion
    // time, so the fixture carries the pretty-printed JSON as literal text.
    let pretty = serde_json::to_string_pretty(&serde_json::json!({
        "path": "Cargo.lock",
        "total_lines": 2,
    }))
    .unwrap();
    let result = history::ext::tool_result("1", Some("read"), pretty, false);
    let text = tool_result_text(&result);
    assert!(
        text.contains("\"path\": \"Cargo.lock\""),
        "unexpected: {text}"
    );
    assert!(text.contains("\"total_lines\": 2"), "unexpected: {text}");
    assert!(!text.contains("Json {"), "Debug rendering leaked: {text}");
}

// ----- permission requests -----

fn selected(option_id: &str) -> RequestPermissionOutcome {
    RequestPermissionOutcome::Selected(
        agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(option_id.to_string()),
    )
}

#[test]
fn permission_options_list_all_four_choices() {
    let options = permission_options();
    let ids: Vec<&str> = options.iter().map(|o| o.option_id.0.as_ref()).collect();
    assert_eq!(
        ids,
        ["allow_once", "allow_always", "reject_once", "reject_always"]
    );
}

#[test]
fn outcomes_map_to_reference_answers() {
    assert_eq!(
        outcome_to_answer(&selected("allow_once")),
        PermissionAnswer::AllowOnce
    );
    assert_eq!(
        outcome_to_answer(&selected("allow_always")),
        PermissionAnswer::AllowSession
    );
    assert_eq!(
        outcome_to_answer(&selected("reject_once")),
        PermissionAnswer::Deny
    );
    assert_eq!(
        outcome_to_answer(&selected("reject_always")),
        PermissionAnswer::DenyAlwaysLocal
    );
    assert_eq!(
        outcome_to_answer(&RequestPermissionOutcome::Cancelled),
        PermissionAnswer::Deny
    );
    assert_eq!(outcome_to_answer(&selected("nope")), PermissionAnswer::Deny);
}

#[test]
fn permission_request_carries_the_call_and_scopes() {
    let request = permission_request(
        &SessionId::new("s1"),
        "call-1",
        "edit",
        &serde_json::json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
        &["/tmp/proj/a.rs".to_string()],
    );
    assert_eq!(request.session_id.0.as_ref(), "s1");
    assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-1");
    assert_eq!(request.tool_call.fields.name.as_deref(), Some("edit"));
    assert_eq!(request.options.len(), 4);
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["options"][0]["optionId"], "allow_once");
}

#[test]
fn gate_decides_run_skip_and_stop_without_a_client() {
    // The engine decides without a client round-trip for allow/deny.
    let tmp = tempfile::tempdir().unwrap();
    let permissions = Arc::new(PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        tmp.path().to_path_buf(),
    ));
    let read_call = history::ContentBlock::ToolUse {
        id: "1".into(),
        name: "read".into(),
        input: serde_json::json!({"path": "x"}),
        thought_signature: None,
    };
    // read-only tool: allowed by default.
    let tool = ToolKey::native("read");
    let history::ContentBlock::ToolUse { input, .. } = &read_call else {
        unreachable!("the fixture call is a ToolUse block");
    };
    let (scopes, force_prompt) = scope_for_call(permissions.cwd(), "read", input);
    assert!(matches!(
        permissions.check_multi(&tool, &scopes, force_prompt),
        PermissionCheck::Allowed
    ));
    // A write with a session allow rule: allowed.
    let edit_tool = ToolKey::native("edit");
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: edit_tool.clone(),
        scope: Some(tmp.path().join("a.rs").display().to_string()),
        effect: crate::permissions::Effect::Allow,
    });
    let (scopes, force_prompt) = scope_for_call(
        permissions.cwd(),
        "edit",
        &serde_json::json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
    );
    assert!(matches!(
        permissions.check_multi(&edit_tool, &scopes, force_prompt),
        PermissionCheck::Allowed
    ));
    // A session deny wins and produces the denial text the model sees.
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: edit_tool.clone(),
        scope: Some(tmp.path().join("b.rs").display().to_string()),
        effect: crate::permissions::Effect::Deny,
    });
    let (scopes, force_prompt) = scope_for_call(
        permissions.cwd(),
        "edit",
        &serde_json::json!({"path": "b.rs", "old_string": "x", "new_string": "y"}),
    );
    let PermissionCheck::Denied = permissions.check_multi(&edit_tool, &scopes, force_prompt) else {
        panic!("expected denial");
    };
    assert!(
        denied_message(&edit_tool, &scopes)
            .starts_with(crate::permissions::PERMISSION_DENIED_PREFIX)
    );
}

// ----- elicitation -----

fn question_specs() -> Vec<QuestionSpec> {
    vec![
        QuestionSpec {
            question: "Pick a framework".into(),
            header: Some("Framework".into()),
            options: vec![
                QuestionOption {
                    label: "axum".into(),
                    description: Some("tokio based".into()),
                },
                QuestionOption {
                    label: "actix".into(),
                    description: None,
                },
            ],
            multi_select: false,
        },
        QuestionSpec {
            question: "Which features?".into(),
            header: Some("Features".into()),
            options: vec![
                QuestionOption {
                    label: "auth".into(),
                    description: None,
                },
                QuestionOption {
                    label: "uploads".into(),
                    description: None,
                },
            ],
            multi_select: true,
        },
        QuestionSpec {
            question: "Anything else?".into(),
            header: None,
            options: vec![],
            multi_select: false,
        },
    ]
}

#[test]
fn form_request_maps_questions_to_schema() {
    let req = form_request(
        &SessionId::new("sess_1"),
        Some("tool_1".to_owned()),
        &question_specs(),
    )
    .unwrap();
    assert_eq!(req.message, "3 questions");

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
    let req = form_request(&SessionId::new("sess_1"), None, &qs).unwrap();
    assert_eq!(req.message, "Proceed?");
}

#[test]
fn form_request_rejects_empty_questions() {
    assert!(form_request(&SessionId::new("sess_1"), None, &[]).is_err());
}

fn accept_response(content: serde_json::Value) -> serde_json::Value {
    if content.is_null() {
        serde_json::json!({"action": "accept"})
    } else {
        serde_json::json!({"action": "accept", "content": content})
    }
}

#[test]
fn accepted_forms_map_answers_by_position() {
    let answer = answer_from_response(&accept_response(
        serde_json::json!({ "q1": "axum", "q2": ["auth", "uploads"] }),
    ));
    assert!(!answer.dismissed);
    assert_eq!(answer.answers, vec![vec!["axum"], vec!["auth", "uploads"]]);
}

#[test]
fn missing_answer_becomes_empty_labels() {
    let answer = answer_from_response(&accept_response(serde_json::json!({ "q2": ["auth"] })));
    assert!(!answer.dismissed);
    assert_eq!(answer.answers, vec![vec![], vec!["auth"]]);
}

#[test]
fn nulled_out_field_costs_one_answer_not_the_form() {
    let answer = answer_from_response(&accept_response(
        serde_json::json!({ "q1": "axum", "q2": null }),
    ));
    assert!(!answer.dismissed);
    assert_eq!(answer.answers, vec![vec!["axum"]]);
}

#[test]
fn non_accept_is_dismissed() {
    for raw in [
        r#"{"action":"decline"}"#,
        r#"{"action":"cancel"}"#,
        r#"{"action":"_custom"}"#,
    ] {
        let response: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(answer_from_response(&response).dismissed, "{raw}");
    }
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

// ----- session/load -----

#[tokio::test]
async fn load_session_restores_history_and_model() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let session_ref =
        crate::id::SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap());
    let mut stored = crate::storage::sessions::StoredSession::new("openai/gpt-x", "/tmp/proj");
    stored.id = session_ref.clone();
    stored.push_message(history::ext::entry(history::ext::stored(
        history::ext::user_text("hello"),
    )));
    stored.save(&dir).unwrap();

    let loaded = crate::storage::sessions::StoredSession::load(session_ref.id(), &dir).unwrap();
    assert_eq!(loaded.model, "openai/gpt-x");
    assert_eq!(loaded.messages().len(), 1);

    // The provider/model resolution from a stored spec (pure helper
    // semantics exercised through load_session's match logic).
    let (provider, model) = match loaded.model.split_once('/') {
        Some((p, m)) if !m.is_empty() => (p.to_owned(), Some(m.to_owned())),
        _ => (String::new(), None),
    };
    assert_eq!(provider, "openai");
    assert_eq!(model.as_deref(), Some("gpt-x"));
}

#[tokio::test]
async fn load_session_rejects_unknown_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let missing =
        crate::id::SessionRef::from_id("01965087-4c71-7f00-8000-000000000001".parse().unwrap());
    assert!(crate::storage::sessions::StoredSession::load(missing.id(), &dir).is_err());
}

/// The `session/load` replay: the shared replay items become the
/// session updates a client rebuilds its transcript from — user and agent
/// text plus completed tool calls. Markers and compaction summaries have
/// no ACP translation live either, so they replay as nothing.
#[test]
fn replay_updates_carry_the_shared_transcript() {
    use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallStatus};

    let items = vec![
        crate::harness::ReplayItem::User {
            text: "do it".into(),
        },
        crate::harness::ReplayItem::ToolResult {
            id: "t1".into(),
            name: "bash".into(),
            text: "done".into(),
            full: None,
            is_error: false,
        },
        crate::harness::ReplayItem::Marker {
            kind: crate::history::marker::CANCEL.into(),
            text: "[Cancelled by user]".into(),
        },
        crate::harness::ReplayItem::Compaction {
            kind: "llm".into(),
            text: "summary".into(),
        },
        crate::harness::ReplayItem::Assistant {
            text: "all set".into(),
        },
    ];
    let updates = replay_updates(&items);
    assert_eq!(updates.len(), 3, "marker and compaction drop out");
    assert!(matches!(
        &updates[0],
        SessionUpdate::UserMessageChunk(chunk) if chunk.content == text_block("do it")
    ));
    let SessionUpdate::ToolCall(call) = &updates[1] else {
        panic!("tool result replays as a tool call, got {:?}", updates[1]);
    };
    assert_eq!(call.tool_call_id.0.as_ref(), "t1");
    assert_eq!(call.status, ToolCallStatus::Completed);
    assert!(matches!(
        call.content.as_slice(),
        [ToolCallContent::Content(content)] if *content == Content::new(text_block("done"))
    ));
    assert!(matches!(
        &updates[2],
        SessionUpdate::AgentMessageChunk(chunk) if chunk.content == text_block("all set")
    ));
}

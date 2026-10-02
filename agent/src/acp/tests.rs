use super::elicitation::*;
use super::permissions::*;
use super::turn::*;
use super::*;
use crate::permissions::{PermissionAnswer, PermissionCheck, ToolKey, scope_for_call};
use crate::run::RunOutcome;
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
    let session = Session {
        compaction: Default::default(),
        workspace: Workspace::new(std::env::temp_dir()).unwrap(),
        instructions: Default::default(),
        history: Vec::new(),
        provider_name: "openai".into(),
        models: vec![CatalogModel {
            id: "gpt-x".into(),
            name: Some("GPT X".into()),
            description: None,
            context_length: None,
            max_output_tokens: None,
        }],
        model: "gpt-x".into(),
        context_length: Some(128_000),
        thinking: Default::default(),
        store: None,
        dedup: run::shared_cache(),
        permissions: Arc::new(PermissionManager::new(
            crate::permissions::PermissionsConfig::default(),
            std::env::temp_dir(),
        )),
        cancel: run::cancel_channel().0,
        turn: None,
    };
    let options = session.config_options(&["openai".into(), "llamafile".into()]);
    assert_eq!(options.len(), 3);
    assert_eq!(options[0].id.0.as_ref(), "provider");
    assert_eq!(options[1].id.0.as_ref(), "model");
    assert_eq!(options[2].id.0.as_ref(), "thinking");
}

fn test_session() -> Session {
    Session {
        compaction: Default::default(),
        workspace: Workspace::new(std::env::temp_dir()).unwrap(),
        instructions: Default::default(),
        history: Vec::new(),
        provider_name: "openai".into(),
        models: Vec::new(),
        model: "gpt-x".into(),
        context_length: Some(128_000),
        thinking: Default::default(),
        store: None,
        dedup: run::shared_cache(),
        permissions: Arc::new(PermissionManager::new(
            crate::permissions::PermissionsConfig::default(),
            std::env::temp_dir(),
        )),
        cancel: run::cancel_channel().0,
        turn: None,
    }
}

#[test]
fn thinking_options_include_current_budget() {
    let option = thinking_option(crate::thinking::ThinkingConfig::Budget(4096));
    let json = serde_json::to_value(option).unwrap();
    let current = crate::thinking::ThinkingConfig::Budget(4096).to_string();
    assert_eq!(json["currentValue"], current);
    let values: Vec<_> = json["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["value"].as_str().unwrap())
        .collect();
    assert_eq!(
        values,
        [
            "off", "adaptive", "minimal", "low", "medium", "high", "xhigh", "max", &current
        ]
    );
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
async fn stale_turn_does_not_commit_history() {
    let session_id = SessionId::new("s1");
    let sessions = Mutex::new(BTreeMap::from([("s1".to_string(), test_session())]));
    sessions
        .lock()
        .await
        .get_mut("s1")
        .unwrap()
        .begin_turn(1)
        .unwrap();
    // Turn 1 is superseded by a newer accepted turn before it finishes.
    sessions.lock().await.get_mut("s1").unwrap().turn = Some(2);
    commit_turn(
        &sessions,
        &session_id,
        1,
        vec![history::Message::system("stale")],
    )
    .await;
    {
        let map = sessions.lock().await;
        let session = map.get("s1").unwrap();
        assert!(session.history.is_empty(), "a stale turn must not commit");
        assert_eq!(session.turn, Some(2), "the newer turn keeps its slot");
    }
    // The current turn still commits and releases the slot.
    commit_turn(
        &sessions,
        &session_id,
        2,
        vec![history::Message::system("fresh")],
    )
    .await;
    {
        let map = sessions.lock().await;
        let session = map.get("s1").unwrap();
        assert_eq!(session.history.len(), 1);
        assert_eq!(session.turn, None);
    }
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
async fn run_turn_tools_route_task_calls_to_the_installed_launcher() {
    use rig_core::test_utils::MockCompletionModel;

    struct AllowAll;
    impl run::BeforeExecute for AllowAll {
        fn decide(&self, _call: history::ToolCall) -> run::BoxFuture<run::Decision> {
            Box::pin(async { run::Decision::Run })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (_flag, cancel) = run::cancel_channel();
    // The launcher run_turn installs per turn (A.5): a `task` call with
    // a bad subagent type reaches its spawn and fails informatively
    // instead of degrading to the no-host availability error.
    let launcher = Arc::new(crate::subagent::SubagentLauncher {
        parent_model: crate::providers::DynamicModel::wrap(
            Some("mock"),
            MockCompletionModel::from_stream_turns(
                Vec::<Vec<rig_core::test_utils::MockStreamEvent>>::new(),
            ),
        ),
        parent_spec: "mock/mock-model".into(),
        provider: "mock".into(),
        providers: Default::default(),
        agent: Default::default(),
        compression: Default::default(),
        base_prompt: String::new(),
        workspace: workspace.clone(),
        history: Vec::new(),
        cancel: cancel.clone(),
        cancels: Arc::new(run::cancel::CancelMap::new()),
        emit: Arc::new(|_| {}),
        before: None,
    });
    let tools = turn_tools(
        workspace,
        Arc::new(crate::tools::DismissAsk),
        launcher,
        Arc::new(AllowAll),
        run::shared_cache(),
    );
    let call = || history::ToolCall {
        id: "1".into(),
        function: history::ToolFunction {
            name: "task".into(),
            arguments: serde_json::json!({
                "description": "probe",
                "prompt": "probe",
                "subagent_type": "bogus",
            }),
        },
    };
    let result = match tools.execute(call()).await {
        Ok(run::DispatchOutcome::Ran(result)) => result,
        other => panic!("the task call must run: {other:?}"),
    };
    assert!(result.is_error);
    assert!(
        history::ToolResultContent::to_text(&result.content[0])
            .contains("unknown subagent type: bogus"),
        "the registered task tool must reach the installed launcher"
    );

    // Counterfactual (the pre-fix ACP behavior): without an installed
    // launcher the same registered call only gets the availability error.
    let bare = Workspace::new(dir.path()).unwrap().register();
    let result = match bare.execute(call()).await {
        Ok(run::DispatchOutcome::Ran(result)) => result,
        other => panic!("the task call must run: {other:?}"),
    };
    assert!(result.is_error);
    assert!(
        history::ToolResultContent::to_text(&result.content[0])
            .contains("task tool is not available in this session"),
        "a missing launcher degrades to the availability error"
    );
}

#[tokio::test]
async fn streamed_filesystem_results_match_model_and_acp_text() {
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
    use serde_json::json;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("file.rs"),
        "skipped\r\n\r\n    let name = \"βeta\";\r\nlast",
    )
    .unwrap();
    let mut turns = Vec::new();
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
        turns.push(vec![
            MockStreamEvent::tool_call(id, name, args),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]);
    }
    turns.push(vec![
        MockStreamEvent::text("done"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]);
    let model = MockCompletionModel::from_stream_turns(turns);
    let tools = Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = run::cancel_channel();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut history = Vec::new();
    let outcome = run::run(
        &model,
        &run::RunParams::default(),
        &tools,
        &mut history,
        "read, search, edit, delete",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "done"));
    let mut results = Vec::new();
    let events = events.lock().unwrap();
    for event in events.iter() {
        if let run::Event::ToolDone { name, result, .. } = event {
            // The model-visible text and the ACP display text must match.
            let ToolCallContent::Content(Content {
                content: ContentBlock::Text(display),
                ..
            }) = tool_result_content(result)
            else {
                panic!("ACP must display tool text");
            };
            assert_eq!(result.content.len(), 1);
            let model_text = history::ToolResultContent::to_text(&result.content[0]);
            assert_eq!(display.text, model_text);
            results.push((name.clone(), model_text));
        }
    }
    assert_eq!(results, [
        ("read".into(), "2: \n3:     let name = \"βeta\";\n\n...\n\nTruncated lines: 4-4. Use offset=4 to read further.".into()),
        ("grep".into(), "file.rs:\n  3:     let name = \"βeta\";".into()),
        ("edit".into(), "edited file.rs\n--- file.rs\n+++ file.rs\n@@ -1 +1 @@\n  skipped\r\n  \r\n-     let name = \"βeta\";\r\n+     let name = \"new\";\r\n  last".into()),
        ("delete".into(), "deleted: file.rs".into()),
    ]);
    // Verify what the next model request actually receives, not only the
    // display events: all four results must remain literal text.
    let requests = model.requests();
    assert_eq!(requests.len(), 5);
    let model_results = crate::edge::rig_to_own(&requests[4].chat_history)
        .iter()
        .flat_map(|message| match message {
            history::Message::User { content } => content
                .iter()
                .filter_map(|item| {
                    if let history::UserContent::ToolResult(result) = item {
                        assert_eq!(result.content.len(), 1);
                        Some((
                            result.name.clone(),
                            history::ToolResultContent::to_text(&result.content[0]),
                        ))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect::<Vec<_>>();
    assert_eq!(model_results, results);
    assert!(!dir.path().join("file.rs").exists());
}

#[test]
fn tool_result_text_preserves_literal_json_and_errors() {
    for text in [r#"{"lines":[],"total_lines":0}"#, "file not found", ""] {
        assert_eq!(
            tool_result_text(&[history::ToolResultContent::text(text)]),
            text
        );
    }
}

#[test]
fn tool_result_text_pretty_prints_json_results() {
    let items = vec![history::ToolResultContent::Json {
        value: serde_json::json!({ "path": "Cargo.lock", "total_lines": 2 }),
    }];
    let text = tool_result_text(&items);
    assert!(
        text.contains("\"path\": \"Cargo.lock\""),
        "unexpected: {text}"
    );
    assert!(text.contains("\"total_lines\": 2"), "unexpected: {text}");
    assert!(!text.contains("Json {"), "Debug rendering leaked: {text}");
}

// ----- G.5 permission requests -----

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
    let read_call = history::ToolCall::new("1", "read", serde_json::json!({"path": "x"}));
    // read-only tool: allowed by default.
    let tool = ToolKey::native("read");
    let (scopes, force_prompt) =
        scope_for_call(permissions.cwd(), "read", &read_call.function.arguments);
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

// ----- G.5 elicitation -----

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

// ----- G.5 session/load -----

#[tokio::test]
async fn load_session_restores_history_and_model() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
    let session_ref =
        crate::id::SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap());
    let mut stored = crate::headless::StoredSession::new("openai/gpt-x", "/tmp/proj");
    stored.id = session_ref.clone();
    stored.push_message(history::Message::User {
        content: vec![history::UserContent::text("hello")],
    });
    stored.save(&dir).unwrap();

    let loaded = crate::headless::StoredSession::load(session_ref.id(), &dir).unwrap();
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
    assert!(crate::headless::StoredSession::load(missing.id(), &dir).is_err());
}

// Tests covering streaming/tool updates, session configs, comments, elicitation
// values, checkpoints, and id generation. Kept together because they share
// the same gpui window scaffolding.
#![cfg(test)]
use super::*;
use agent_client_protocol::schema::v1::{
    Content, ContentBlock, ContentChunk, Diff as AcpDiff, ElicitationContentValue,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionUpdate,
    TextContent, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

fn calls(message: &Message) -> Vec<ToolCall> {
    message.body.tool_calls().cloned().collect()
}

fn tool_text(text: &str) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

fn tool_test_app(cx: &mut Context<App>) -> App {
    let mut app = App::from_state(PersistedState::default(), vec![], None, cx);
    let project = Project {
        id: "project".into(),
        name: "Test".into(),
        path: ".".into(),
        desc: String::new(),
        updated: String::new(),
        checkpoint_label: String::new(),
        model: String::new(),
    };
    app.sessions_by_project.insert(
        project.id.clone(),
        vec![Session {
            id: "session".into(),
            name: "Test".into(),
            messages: vec![Message {
                id: "tools".into(),
                role: Role::Assistant,
                body: MessageBody::default(),
                time: None,
                context: vec![],
                attached_comments: vec![],
                checkpoint_label: None,
                steps: None,
                diff: None,
                terminal: None,
            }],
            acp_session_id: None,
            archived: false,
            agent_profile_id: None,
        }],
    );
    app.active_project = Some(project);
    app.active_session_id = Some("session".into());
    app.screen = Screen::Workspace;
    app
}

fn assistant_text(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

#[gpui::test]
fn assistant_text_and_tools_keep_stream_order_after_updates_and_reload(
    cx: &mut gpui::TestAppContext,
) {
    let (app, cx) = cx.add_window_view(|_, cx| tool_test_app(cx));
    app.update(cx, |app, cx| {
        app.apply_session_update(assistant_text("**Before"));
        app.apply_session_update(assistant_text(" reading**."));
        app.apply_session_update(SessionUpdate::ToolCall(
            ToolCall::new("first", "read a.rs").status(ToolCallStatus::InProgress),
        ));
        app.apply_session_update(assistant_text("Between "));
        app.apply_session_update(assistant_text("`calls`."));
        app.apply_session_update(SessionUpdate::ToolCall(
            ToolCall::new("second", "read b.rs").status(ToolCallStatus::InProgress),
        ));
        app.apply_session_update(assistant_text(""));
        // Completion events update the original slot, not the current tail.
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "second",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![tool_text("second result")]),
        )));
        app.apply_session_update(assistant_text("After "));
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "first",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![tool_text("first result")]),
        )));
        app.apply_session_update(assistant_text("both."));
        let messages = app.active_messages();
        let tool_calls = calls(&messages[0]);
        assert_eq!(
            messages[0].body.parts,
            vec![
                MessagePart::Text("**Before reading**.".into()),
                MessagePart::ToolCall(Box::new(tool_calls[0].clone())),
                MessagePart::Text("Between `calls`.".into()),
                MessagePart::ToolCall(Box::new(tool_calls[1].clone())),
                MessagePart::Text("After both.".into()),
            ]
        );
        assert_eq!(tool_calls[0].content, vec![tool_text("first result")]);
        assert_eq!(tool_calls[1].content, vec![tool_text("second result")]);
        let saved = serde_json::to_value(&messages).unwrap();
        assert!(saved[0].get("text").is_none());
        assert!(saved[0].get("tool_calls").is_none());
        let restored: Vec<Message> = serde_json::from_value(saved).unwrap();
        assert_eq!(restored[0].body, messages[0].body);
        app.update_active_messages(restored);
        cx.notify();
    });
    cx.run_until_parked();
    for width in [1600., 800.] {
        cx.simulate_resize(gpui::size(px(width), px(600.)));
        cx.run_until_parked();
        let card = cx.debug_bounds("assistant-card-tools").unwrap();
        let mut bottom = card.origin.y;
        for selector in [
            "assistant-markdown-tools",
            "tool-call-tools-first",
            "assistant-markdown-tools-text-1",
            "tool-call-tools-second",
            "assistant-markdown-tools-text-2",
        ] {
            let bounds = cx.debug_bounds(selector).unwrap();
            assert!(bounds.origin.y >= bottom, "out of order: {selector}");
            bottom = bounds.bottom();
        }
        assert!(bottom < card.bottom());
    }
}

#[gpui::test]
fn multiple_tool_calls_render_separately_and_updates_preserve_other_calls(
    cx: &mut gpui::TestAppContext,
) {
    let (app, cx) = cx.add_window_view(|_, cx| tool_test_app(cx));
    app.update(cx, |app, cx| {
        for (id, title) in [("first", "read a.rs"), ("second", "read b.rs")] {
            app.apply_session_update(SessionUpdate::ToolCall(
                ToolCall::new(id, title).status(ToolCallStatus::InProgress),
            ));
        }
        // Finish in reverse order; one call can contain several content blocks.
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "second",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![
                    tool_text("1: second file"),
                    tool_text("2: more output"),
                ]),
        )));
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "first",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![tool_text("1: first file")]),
        )));
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "second",
            ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
        )));
        let message = app.active_messages().remove(0);
        assert_eq!(calls(&message).len(), 2);
        assert_eq!(calls(&message)[0].title, "read a.rs");
        assert_eq!(calls(&message)[0].content, vec![tool_text("1: first file")]);
        assert_eq!(calls(&message)[1].title, "read b.rs");
        assert_eq!(calls(&message)[1].status, ToolCallStatus::Failed);
        assert_eq!(
            calls(&message)[1].content,
            vec![tool_text("1: second file"), tool_text("2: more output")]
        );
        assert!(message.terminal.is_none());
        // Layout assertions below read content bounds, so expand the calls.
        app.expanded_tool_calls.extend([
            "tool-call-tools-first".to_owned(),
            "tool-call-tools-second".to_owned(),
        ]);
        cx.notify();
    });
    cx.run_until_parked();
    for width in [1600., 800.] {
        cx.simulate_resize(gpui::size(px(width), px(600.)));
        cx.run_until_parked();
        let first = cx.debug_bounds("tool-call-tools-first").unwrap();
        let second = cx.debug_bounds("tool-call-tools-second").unwrap();
        let first_text = cx.debug_bounds("tool-call-tools-first-content-0").unwrap();
        let second_text = cx.debug_bounds("tool-call-tools-second-content-1").unwrap();
        let assistant = cx.debug_bounds("assistant-card-tools").unwrap();
        assert!(first.bottom() <= second.origin.y);
        assert!(first_text.bottom() < first.bottom());
        assert!(second_text.bottom() < second.bottom());
        assert!(second.bottom() < assistant.bottom());
    }
    // Title-only updates preserve content, while an explicit empty list clears it.
    app.update(cx, |app, _| {
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "first",
            ToolCallUpdateFields::new().title("renamed read"),
        )));
        assert_eq!(
            calls(&app.active_messages()[0])[0].content,
            vec![tool_text("1: first file")]
        );
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "first",
            ToolCallUpdateFields::new().content(vec![]),
        )));
        let calls = calls(&app.active_messages().remove(0));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].title, "renamed read");
        assert!(calls[0].content.is_empty());
        assert_eq!(calls[1].content.len(), 2);
    });
}

#[gpui::test]
fn tool_updates_keep_their_message_owner_and_multiple_diffs(cx: &mut gpui::TestAppContext) {
    let (app, cx) = cx.add_window_view(|_, cx| tool_test_app(cx));
    app.update(cx, |app, cx| {
        // Missing start notifications still get their own placeholder call.
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "edit",
            ToolCallUpdateFields::new().content(vec![
                ToolCallContent::Diff(AcpDiff::new("a.rs", "new a").old_text("old a")),
                tool_text("edited files"),
                ToolCallContent::Diff(AcpDiff::new("b.rs", "new b").old_text("old b")),
            ]),
        )));
        let mut messages = app.active_messages();
        assert_eq!(calls(&messages[0])[0].title, "Agent operation");
        assert_eq!(calls(&messages[0])[0].content.len(), 3);
        assert!(messages[0].diff.is_none());
        assert!(app.file_diffs.contains_key("a.rs"));
        assert!(app.file_diffs.contains_key("b.rs"));
        let mut later = messages[0].clone();
        later.id = "later".into();
        later.body.parts.clear();
        messages.push(later);
        app.update_active_messages(messages);
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "edit",
            ToolCallUpdateFields::new()
                .title("edit two files")
                .status(ToolCallStatus::Completed),
        )));
        let messages = app.active_messages();
        assert_eq!(calls(&messages[0])[0].title, "edit two files");
        assert_eq!(calls(&messages[0])[0].content.len(), 3);
        assert!(calls(&messages[1]).is_empty());
        let saved = serde_json::to_string(&messages).unwrap();
        let restored: Vec<Message> = serde_json::from_str(&saved).unwrap();
        assert_eq!(calls(&restored[0]), calls(&messages[0]));
        // The diff assertions below require the collapsed call to be open.
        app.expanded_tool_calls
            .insert("tool-call-tools-edit".to_owned());
        cx.notify();
    });
    cx.run_until_parked();
    let first = cx
        .debug_bounds("diff-text-session:session:tool-call-tools-edit-content-0_0")
        .unwrap();
    let second = cx
        .debug_bounds("diff-text-session:session:tool-call-tools-edit-content-2_0")
        .unwrap();
    assert!(first.bottom() < second.origin.y);
}

#[gpui::test]
fn tool_call_ids_are_scoped_to_the_active_session(cx: &mut gpui::TestAppContext) {
    let (app, cx) = cx.add_window_view(|_, cx| tool_test_app(cx));
    app.update(cx, |app, _| {
        let mut other = app.sessions_by_project["project"][0].clone();
        other.id = "other".into();
        app.sessions_by_project
            .get_mut("project")
            .unwrap()
            .push(other);
        app.apply_session_update(SessionUpdate::ToolCall(
            ToolCall::new("read", "first session").content(vec![tool_text("first")]),
        ));
        app.active_session_id = Some("other".into());
        app.apply_session_update(SessionUpdate::ToolCall(
            ToolCall::new("read", "other session").content(vec![tool_text("other")]),
        ));
        app.apply_session_update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "read",
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        )));
        let sessions = &app.sessions_by_project["project"];
        assert_eq!(calls(&sessions[0].messages[0])[0].title, "first session");
        assert_eq!(
            calls(&sessions[0].messages[0])[0].status,
            ToolCallStatus::Pending
        );
        assert_eq!(calls(&sessions[1].messages[0])[0].title, "other session");
        assert_eq!(
            calls(&sessions[1].messages[0])[0].status,
            ToolCallStatus::Completed
        );
    });
}

#[gpui::test]
fn assistant_content_and_footers_stay_inside_scrollable_cards(cx: &mut gpui::TestAppContext) {
    let (app, cx) = cx.add_window_view(|_, cx| {
            let mut app = App::from_state(PersistedState::default(), vec![], None, cx);
            let project = Project {
                id: "project".into(),
                name: "Test".into(),
                path: ".".into(),
                desc: String::new(),
                updated: String::new(),
                checkpoint_label: String::new(),
                model: String::new(),
            };
            let messages = ["completed", "streaming"]
                .into_iter()
                .map(|id| {
                    let mut body = MessageBody::default();
                    for call in [
                        ToolCall::new("read", "read long output")
                            .status(ToolCallStatus::Completed)
                            .content(vec![tool_text(&"Long tool output ".repeat(100))]),
                        ToolCall::new("grep", "grep results")
                            .status(ToolCallStatus::Completed)
                            .content(vec![tool_text(&"src/main.rs:\n  1: matching line\n".repeat(12))]),
                    ] {
                        body.update_tool_call(call.into());
                    }
                    body.push_text(&format!(
                        "## Root cause\n\n{}\n\n1. The read tool returns structured output with a path and numbered lines.\n2. Render the JSON rather than its **Debug representation**.\n\n## Fix\n\n```json\n{{\n  \"path\": \"Cargo.lock\",\n  \"lines\": [],\n  \"total_lines\": 11485,\n  \"next_offset\": 35\n}}\n```\n\n**One caveat:** {}",
                        "A paragraph with **formatted text** and `inline code` that wraps across many lines in a narrow conversation pane. ".repeat(5),
                        "This final paragraph after the code block must stay inside the card and remain reachable by scrolling. ".repeat(5),
                    ));
                    Message {
                        id: id.into(),
                        role: Role::Assistant,
                        body,
                        time: None,
                        context: vec![],
                        attached_comments: vec![],
                        checkpoint_label: (id == "completed").then(|| "Checkpoint 11".into()),
                        steps: None,
                        diff: None,
                        terminal: None,
                    }
                })
                .collect();
            app.sessions_by_project.insert(
                project.id.clone(),
                vec![Session {
                    id: "session".into(),
                    name: "Test".into(),
                    messages,
                    acp_session_id: None,
                    archived: false,
                    agent_profile_id: None,
                }],
            );
            app.active_project = Some(project);
            app.active_session_id = Some("session".into());
            app.screen = Screen::Workspace;
            app.thinking = true;
            app
        });

    for width in [1600., 1100., 800.] {
        cx.simulate_resize(gpui::size(px(width), px(2000.)));
        cx.run_until_parked();
        let completed_height = cx
            .debug_bounds("assistant-card-completed")
            .unwrap()
            .size
            .height;
        let streaming_height = cx
            .debug_bounds("assistant-card-streaming")
            .unwrap()
            .size
            .height;
        cx.simulate_resize(gpui::size(px(width), px(600.)));
        cx.run_until_parked();
        // Bring each footer into view using the real thread scroll handle.
        for (card_selector, footer_selector, last_paragraph_selector) in [
            (
                "assistant-card-completed",
                "checkpoint-completed",
                "assistant-markdown-completed-paragraph-6",
            ),
            (
                "assistant-card-streaming",
                "running-indicator",
                "assistant-markdown-streaming-paragraph-6",
            ),
        ] {
            let footer = cx.debug_bounds(footer_selector).unwrap();
            let initial_card = cx.debug_bounds(card_selector).unwrap();
            assert!(footer.bottom() <= initial_card.bottom() - px(16.));
            assert_eq!(
                initial_card.size.height,
                if card_selector == "assistant-card-completed" {
                    completed_height
                } else {
                    streaming_height
                },
                "Scrolling must not compress a card"
            );
            app.update(cx, |app, cx| {
                let offset = app.thread_scroll.offset();
                app.thread_scroll
                    .set_offset(gpui::point(px(0.), offset.y - footer.bottom() + px(450.)));
                cx.notify();
            });
            cx.run_until_parked();
            let card = cx.debug_bounds(card_selector).unwrap();
            let footer = cx.debug_bounds(footer_selector).unwrap();
            assert!(
                footer.bottom() <= card.bottom() - px(16.),
                "{footer_selector} must fit inside the card's bottom padding: {footer:?}, {card:?}"
            );
            assert!(footer.origin.y >= card.origin.y + px(16.));
            let last_paragraph = cx.debug_bounds(last_paragraph_selector).unwrap();
            assert!(last_paragraph.bottom() <= footer.origin.y - px(12.));
            // The actual end of the text, not just its parent, is visible after
            // scrolling: below the top bar and above the window's status bar.
            let viewport = app.read_with(cx, |app, _| app.thread_scroll.bounds());
            assert!(last_paragraph.bottom() > viewport.origin.y);
            assert!(
                footer.bottom() < viewport.bottom(),
                "{width} {footer_selector}: {footer:?}, card: {card:?}"
            );
        }
        let card = cx.debug_bounds("assistant-card-streaming").unwrap();
        let stop = cx.debug_bounds("cancel-turn").unwrap();
        let composer = cx.debug_bounds("composer").unwrap();
        assert!(stop.bottom() <= card.bottom() - px(16.));
        assert!(composer.origin.y >= card.bottom() + px(16.));
    }
}

#[gpui::test]
fn selecting_markdown_then_typing_opens_and_focuses_a_comment(cx: &mut gpui::TestAppContext) {
    assert_type_to_comment("assistant", cx);
}

#[gpui::test]
fn selecting_text_after_a_tool_call_opens_a_comment(cx: &mut gpui::TestAppContext) {
    assert_type_to_comment("interleaved", cx);
}

#[gpui::test]
fn selecting_user_text_then_typing_opens_a_comment(cx: &mut gpui::TestAppContext) {
    assert_type_to_comment("user", cx);
}

#[gpui::test]
fn selecting_inline_diff_then_typing_opens_a_comment(cx: &mut gpui::TestAppContext) {
    assert_type_to_comment("inline-diff", cx);
}

#[gpui::test]
fn selecting_file_diff_then_typing_opens_a_comment(cx: &mut gpui::TestAppContext) {
    assert_type_to_comment("file-diff", cx);
}

fn assert_type_to_comment(surface: &str, cx: &mut gpui::TestAppContext) {
    let (app, cx) = cx.add_window_view(|_, cx| {
        // Build the real app without reading or writing the user's config.
        let mut app = App::from_state(PersistedState::default(), vec![], None, cx);
        let project = Project {
            id: "project".into(),
            name: "Test".into(),
            path: ".".into(),
            desc: String::new(),
            updated: String::new(),
            checkpoint_label: String::new(),
            model: String::new(),
        };
        app.sessions_by_project.insert(
            project.id.clone(),
            vec![Session {
                id: "session".into(),
                name: "Test".into(),
                messages: vec![Message {
                    id: "selection".into(),
                    role: if surface == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    body: String::from("**selected text**").into(),
                    time: None,
                    context: vec![],
                    attached_comments: vec![],
                    checkpoint_label: None,
                    steps: None,
                    diff: None,
                    terminal: None,
                }],
                acp_session_id: None,
                archived: false,
                agent_profile_id: None,
            }],
        );
        app.active_project = Some(project);
        app.active_session_id = Some("session".into());
        app.screen = Screen::Workspace;
        let diff = Diff {
            file: "main.rs".into(),
            stat: "+1".into(),
            hunk_header: "@@ -0,0 +1 @@".into(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Add,
                text: "selected text".into(),
            }],
        };
        if surface == "inline-diff" {
            app.sessions_by_project.get_mut("project").unwrap()[0].messages[0].diff = Some(diff);
        } else if surface == "file-diff" {
            app.file_diffs.insert(diff.file.clone(), diff);
            app.active_diff_file = Some("main.rs".into());
        } else if surface == "interleaved" {
            let body = &mut app.sessions_by_project.get_mut("project").unwrap()[0].messages[0].body;
            body.parts.insert(
                0,
                MessagePart::ToolCall(Box::new(
                    ToolCall::new("read", "read main.rs").status(ToolCallStatus::Completed),
                )),
            );
            body.parts
                .insert(0, MessagePart::Text("Before reading.".into()));
        }
        app
    });
    let (selector, anchor) = match surface {
        "user" => ("user-markdown-selection", "msg_selection"),
        "interleaved" => ("assistant-markdown-selection-text-1", "msg_selection"),
        "inline-diff" => ("diff-text-session:session:selection_0", "selection_0"),
        "file-diff" => ("diff-text-session:session:f_main.rs_0", "f_main.rs_0"),
        _ => ("assistant-markdown-selection", "msg_selection"),
    };
    let bounds = cx.debug_bounds(selector).unwrap();
    let position = bounds.origin + gpui::point(px(0.), px(5.));
    cx.simulate_mouse_move(position, None, Default::default());
    cx.simulate_mouse_down(position, gpui::MouseButton::Left, Default::default());
    let end = gpui::point(bounds.right() - px(1.), position.y);
    cx.simulate_mouse_move(end, gpui::MouseButton::Left, Default::default());
    cx.simulate_mouse_up(end, gpui::MouseButton::Left, Default::default());
    cx.update(|window, cx| {
        assert!(app.read(cx).selection_focus.is_focused(window));
    });
    cx.simulate_keystrokes("cmd-c");
    assert_eq!(
        cx.read_from_clipboard().unwrap().text().unwrap(),
        "selected text"
    );
    cx.update(|_, cx| assert!(app.read(cx).comment_drafts.is_empty()));
    cx.simulate_keystrokes("h i");
    cx.update(|window, cx| {
        let app = app.read(cx);
        let draft = app.comment_drafts.get(&app.comment_key(anchor)).unwrap();
        assert_eq!(draft.selections, ["selected text"]);
        assert_eq!(draft.input.read(cx).content, "hi");
        assert!(draft.input.read(cx).focus_handle.is_focused(window));
        assert!(app.composer.read(cx).content.is_empty());
    });
    // Focusing another input must not reopen or continue the comment.
    cx.update(|window, cx| {
        window.focus(&app.read(cx).composer.read(cx).focus_handle);
    });
    cx.simulate_keystrokes("x");
    cx.update(|_, cx| {
        let app = app.read(cx);
        assert_eq!(app.composer.read(cx).content, "x");
        assert_eq!(
            app.comment_drafts
                .get(&app.comment_key(anchor))
                .unwrap()
                .input
                .read(cx)
                .content,
            "hi"
        );
    });
    // Escape must preserve a nonempty draft, then close it once emptied.
    let input = cx.update(|window, cx| {
        let app = app.read(cx);
        let input = app
            .comment_drafts
            .get(&app.comment_key(anchor))
            .unwrap()
            .input
            .clone();
        window.focus(&input.read(cx).focus_handle);
        input
    });
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| {
        assert_eq!(input.read(cx).content, "hi");
        assert!(
            app.read(cx)
                .comment_drafts
                .contains_key(&app.read(cx).comment_key(anchor))
        );
    });
    cx.update(|window, cx| window.focus(&input.read(cx).focus_handle));
    cx.simulate_keystrokes("cmd-a backspace escape");
    cx.update(|window, cx| {
        assert!(app.read(cx).comment_drafts.is_empty());
        assert!(app.read(cx).comments.is_empty());
        assert!(app.read(cx).selection_focus.is_focused(window));
    });

    // A manually opened, untouched comment uses the same cancellation path.
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let key = app.comment_key(anchor);
            app.open_comment_box(key.clone(), "test reference".into(), cx);
            window.focus(&app.comment_drafts[&key].input.read(cx).focus_handle);
        });
    });
    cx.simulate_keystrokes("escape");
    cx.update(|_, cx| assert!(app.read(cx).comment_drafts.is_empty()));
}

#[test]
fn selection_comments_preserve_complete_multiline_references() {
    let reference = comment_reference_label(
        "src/main.rs line 2",
        &[
            "let café = 1;\n  café + 1".to_string(),
            "another selection".to_string(),
        ],
    );
    assert_eq!(
        reference,
        "src/main.rs line 2\nSelected text:\n> let café = 1;\n>   café + 1\n\
             \nSelected text:\n> another selection\n"
    );
}

#[test]
fn ordinary_comments_keep_their_original_label() {
    assert_eq!(
        comment_reference_label("assistant reply", &[]),
        "assistant reply"
    );
}

#[test]
fn preserves_each_acp_session_option_as_a_distinct_control() {
    let controls = session_config_controls(vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "large",
            vec![
                SessionConfigSelectOption::new("small", "Small"),
                SessionConfigSelectOption::new("large", "Large"),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "thought-level",
            "Thought level",
            "high",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        ),
        SessionConfigOption::boolean("auto-format", "Auto format", true),
    ]);

    assert_eq!(controls.len(), 3);
    assert_eq!(controls[0].id, "model");
    assert_eq!(controls[0].selected_name, "Large");
    assert!(controls[0].searchable);
    assert_eq!(controls[1].id, "thought-level");
    assert_eq!(controls[1].selected_name, "High");
    assert!(!controls[1].searchable);
    assert_eq!(controls[2].id, "auto-format");
    assert_eq!(controls[2].selected_name, "On");
}

#[test]
fn renders_structured_acp_diff_with_line_kinds_and_stats() {
    let rendered =
        render_acp_diff(AcpDiff::new("/tmp/main.rs", "same\nnew\n").old_text("same\nold\n"));

    assert_eq!(rendered.file, "/tmp/main.rs");
    assert_eq!(rendered.stat, "+1 -1");
    assert_eq!(rendered.lines.len(), 3);
    assert!(matches!(rendered.lines[0].kind, DiffLineKind::Ctx));
    assert_eq!(rendered.lines[0].text, "same");
    assert!(matches!(rendered.lines[1].kind, DiffLineKind::Del));
    assert_eq!(rendered.lines[1].text, "old");
    assert!(matches!(rendered.lines[2].kind, DiffLineKind::Add));
    assert_eq!(rendered.lines[2].text, "new");
}

#[test]
fn converts_supported_json_elicitation_values() {
    assert_eq!(
        json_elicitation_value(serde_json::json!("answer")),
        Some(ElicitationContentValue::String("answer".into()))
    );
    assert_eq!(
        json_elicitation_value(serde_json::json!(42)),
        Some(ElicitationContentValue::Integer(42))
    );
    assert_eq!(
        json_elicitation_value(serde_json::json!(3.5)),
        Some(ElicitationContentValue::Number(3.5))
    );
    assert_eq!(
        json_elicitation_value(serde_json::json!(true)),
        Some(ElicitationContentValue::Boolean(true))
    );
    assert_eq!(
        json_elicitation_value(serde_json::json!(["one", "two"])),
        Some(ElicitationContentValue::StringArray(vec![
            "one".into(),
            "two".into()
        ]))
    );
}

#[test]
fn rejects_json_values_not_supported_by_acp_elicitation() {
    for value in [
        serde_json::Value::Null,
        serde_json::json!({"nested": "object"}),
        serde_json::json!(["text", 2]),
    ] {
        assert_eq!(json_elicitation_value(value), None);
    }
}

#[test]
fn comment_keys_are_isolated_by_session() {
    let first = scoped_comment_key(Some("session-1"), "f_src/main.rs_4");
    let second = scoped_comment_key(Some("session-2"), "f_src/main.rs_4");

    assert_ne!(first, second);
    assert!(first.starts_with(&comment_scope_prefix(Some("session-1"))));
    assert!(!first.starts_with(&comment_scope_prefix(Some("session-2"))));
}

#[test]
fn unscoped_legacy_comments_do_not_match_an_active_session() {
    let legacy_key = "f_src/main.rs_4";

    assert!(!legacy_key.starts_with(&comment_scope_prefix(Some("session-1"))));
}

#[test]
fn rapidly_generated_ids_are_unique() {
    let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_id("s")).collect();
    assert_eq!(ids.len(), 1000);
    assert!(ids.iter().all(|id| id.starts_with('s')));
}

fn assistant_message(id: &str, checkpoint_label: Option<&str>) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        body: String::from("done").into(),
        time: None,
        context: vec![],
        attached_comments: vec![],
        checkpoint_label: checkpoint_label.map(str::to_string),
        steps: None,
        diff: None,
        terminal: None,
    }
}

#[test]
fn checkpoint_numbering_restarts_per_session() {
    // A session's first checkpoint is numbered 0.
    assert_eq!(next_checkpoint_label(&[]), "Checkpoint 0");

    // Numbers follow the checkpoints made in this session alone; the
    // labels other sessions used do not leak in.
    let messages = vec![
        Message {
            id: "user".into(),
            role: Role::User,
            body: String::from("hello").into(),
            time: None,
            context: vec![],
            attached_comments: vec![],
            checkpoint_label: None,
            steps: None,
            diff: None,
            terminal: None,
        },
        assistant_message("a1", Some("Checkpoint 0")),
        assistant_message("a2", Some("Checkpoint 1")),
    ];
    assert_eq!(next_checkpoint_label(&messages), "Checkpoint 2");
}

#[test]
fn restore_finds_the_checkpoint_made_in_the_active_session() {
    let checkpoint = |label: &str, session: Option<&str>, commit: &str| Checkpoint {
        label: label.into(),
        commit: commit.into(),
        session_id: session.map(str::to_string),
    };
    // Every session counts from 0, so labels repeat across sessions.
    let checkpoints = vec![
        checkpoint("Checkpoint 0", Some("session-1"), "commit-1"),
        checkpoint("Checkpoint 0", Some("session-2"), "commit-2"),
    ];

    assert_eq!(
        find_checkpoint(&checkpoints, "Checkpoint 0", Some("session-1"))
            .unwrap()
            .commit,
        "commit-1"
    );
    assert_eq!(
        find_checkpoint(&checkpoints, "Checkpoint 0", Some("session-2"))
            .unwrap()
            .commit,
        "commit-2"
    );
    assert!(find_checkpoint(&checkpoints, "Checkpoint 9", Some("session-1")).is_none());

    // Checkpoints saved before per-session numbering have no session and
    // stay restorable by label.
    let legacy = vec![checkpoint("Checkpoint 1", None, "legacy")];
    assert_eq!(
        find_checkpoint(&legacy, "Checkpoint 1", Some("session-1"))
            .unwrap()
            .commit,
        "legacy"
    );
}

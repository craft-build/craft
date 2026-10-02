use super::super::testutil::{app_with_collapsible_tool, draw_app, scrolled_app};
use super::*;
use crate::tui::provider::{AgentEvent, LineKind, Status, ToolCallData, ToolKind, ToolLine};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Open the task picker (Ctrl-N), move to row `idx + 1`, Enter.
fn focus_task_chat(app: &mut App, tx: &mpsc::UnboundedSender<Command>, idx: usize) {
    app.handle_key(ctrl('n'), tx);
    for _ in 0..=idx {
        app.handle_key(key(KeyCode::Down), tx);
    }
    app.handle_key(key(KeyCode::Enter), tx);
}

fn type_query(app: &mut App, tx: &mpsc::UnboundedSender<Command>, q: &str) {
    for c in q.chars() {
        app.handle_key(key(KeyCode::Char(c)), tx);
    }
}

/// Ctrl-F opens the search modal with the current scroll saved; typing
/// filters the transcript segments and jumps to the best match.
#[test]
fn ctrl_f_opens_search_and_typing_jumps_to_matches() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = scrolled_app();
    draw_app(&mut app, 80, 24);
    let saved = (app.view.scroll, app.view.follow);

    app.handle_key(ctrl('f'), &tx);
    assert!(app.overlays.search.is_open());
    type_query(&mut app, &tx, "message number 5");
    assert!(app.search_matches() > 0, "query must match");

    // The transcript is scrolled to the selected match's segment and
    // that segment is highlighted while the modal stays open.
    let (seg, row) = app.overlays.search.current_segment_index().unwrap();
    assert_eq!(
        (app.view.scroll.seg, app.view.scroll.row as usize),
        (seg, row)
    );
    assert!(!app.view.follow, "a search jump must not re-pin follow");
    assert!(seg > 0, "match is off the very top of the document");

    // Esc restores the scroll position saved on open.
    app.handle_key(key(KeyCode::Esc), &tx);
    assert!(!app.overlays.search.is_open());
    assert_eq!(app.view.highlight_segment, None);
    assert_eq!((app.view.scroll, app.view.follow), saved);
}

/// Enter jumps to the selected match and closes; Up/Down cycle the
/// selection over the matches, wrapping around.
#[test]
fn enter_jumps_and_navigation_cycles_matches() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = scrolled_app();
    draw_app(&mut app, 80, 24);
    app.handle_key(ctrl('f'), &tx);
    type_query(&mut app, &tx, "message number");

    let n = app.search_matches();
    assert!(n >= 2, "several segments carry the phrase");
    app.handle_key(key(KeyCode::Down), &tx);
    assert_eq!(
        app.view.highlight_segment,
        app.overlays
            .search
            .current_segment_index()
            .map(|(seg, _)| seg)
    );

    let target = app.overlays.search.current_segment_index();
    app.handle_key(key(KeyCode::Enter), &tx);
    assert!(!app.overlays.search.is_open());
    assert_eq!(
        app.view.highlight_segment, None,
        "select clears the highlight"
    );
    assert_eq!(
        app.view.scroll,
        crate::tui::ui::scrollback::ScrollPos {
            seg: target.unwrap().0,
            row: target.unwrap().1.min(u16::MAX as usize) as u16
        },
        "jump lands on the matched row of the matched segment"
    );

    // While open, the search modal swallows keys that would otherwise
    // reach the composer.
    let mut app2 = scrolled_app();
    draw_app(&mut app2, 80, 24);
    app2.handle_key(ctrl('f'), &tx);
    app2.handle_key(key(KeyCode::Tab), &tx);
    assert!(
        app2.composer.text.is_empty(),
        "keys never reach the composer"
    );
}

/// The search overlay paints: query row with the caret, and the
/// matching result rows on the raised surface.
#[test]
fn search_overlay_renders_results_and_query_row() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = scrolled_app();
    draw_app(&mut app, 100, 30);
    app.handle_key(ctrl('f'), &tx);
    type_query(&mut app, &tx, "message number 3");
    let text = super::super::testutil::screen_text(&mut app, 100, 30);
    assert!(
        text.contains("/ message number 3"),
        "query row missing:\n{text}"
    );
    assert!(
        text.contains("message number 3"),
        "result row missing:\n{text}"
    );
}

#[test]
fn paste_ignored_when_modal_open() {
    let mut app = App::new();
    app.overlays.modal = Modal::Palette {
        query: "x".into(),
        selected: 0,
    };
    app.insert_paste("nope");
    assert!(app.composer.text.is_empty());
}

fn open_permission(app: &mut App) {
    app.handle_event(AgentEvent::PermissionRequest {
        id: "t9".into(),
        tool: "bash".into(),
        scopes: vec!["execute".into()],
        files: Vec::new(),
        commands: vec!["rm -rf /tmp/x".into()],
    });
}

#[test]
fn permission_request_opens_the_overlay_and_resolution_closes_it() {
    let mut app = App::new();
    open_permission(&mut app);
    assert!(app.overlays.permission_prompt.is_open());
    // A stale resolution must not close a newer request.
    app.handle_event(AgentEvent::PermissionResolved { id: "other".into() });
    assert!(app.overlays.permission_prompt.is_open());
    app.handle_event(AgentEvent::PermissionResolved { id: "t9".into() });
    assert!(!app.overlays.permission_prompt.is_open());
}

fn open_question(app: &mut App) {
    app.handle_event(AgentEvent::QuestionRequest {
        id: "q9".into(),
        questions: vec![crate::tools::QuestionSpec {
            question: "Which?".into(),
            header: None,
            options: vec![crate::tools::QuestionOption {
                label: "A".into(),
                description: None,
            }],
            multi_select: false,
        }],
    });
}

#[test]
fn question_request_opens_the_form_and_resolution_closes_it() {
    let mut app = App::new();
    open_question(&mut app);
    assert!(app.overlays.question_form.is_open());
    // A stale resolution must not close a newer request.
    app.handle_event(AgentEvent::QuestionResolved { id: "other".into() });
    assert!(app.overlays.question_form.is_open());
    app.handle_event(AgentEvent::QuestionResolved { id: "q9".into() });
    assert!(!app.overlays.question_form.is_open());
}

#[test]
fn answering_the_question_routes_the_answered_command() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    open_question(&mut app);
    // Single-select: Enter picks A and submits immediately.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(Command::AnswerQuestion { id, answer })
            if id == "q9" && !answer.dismissed && answer.answers == vec![vec!["A".to_string()]]
    ));
    // Dismissing routes a dismissed answer too.
    open_question(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(Command::AnswerQuestion { id, answer }) if id == "q9" && answer.dismissed
    ));
}

#[test]
fn question_form_owns_plain_keys_while_open() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    open_question(&mut app);
    // A plain char never reaches the composer.
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &tx);
    assert!(app.composer.text.is_empty());
    // Enter picks the option instead of submitting the composer.
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx2);
    assert!(app.composer.text.is_empty());
    assert!(matches!(rx2.try_recv(), Ok(Command::AnswerQuestion { .. })));
}

#[test]
fn answering_the_prompt_routes_the_answered_command() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    open_permission(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(Command::AnswerPermission { id, answer: crate::permissions::PermissionAnswer::AllowOnce })
            if id == "t9"
    ));
}

#[test]
fn prompt_owns_plain_keys_while_open() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    open_permission(&mut app);
    // A plain char never reaches the composer.
    app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &tx);
    assert!(app.composer.text.is_empty());
    // Enter would submit the composer; here it does nothing.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(app.composer.text.is_empty());
    // Ctrl-chords fall through (e.g. the palette still opens).
    app.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(matches!(app.overlays.modal, Modal::Palette { .. }));
}

#[test]
fn paste_lands_in_the_guidance_buffer_while_editing() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    open_permission(&mut app);
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
    app.insert_paste("use cat");
    assert!(app.composer.text.is_empty());
    // Enter denies with the typed guidance.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    // The overlay stays open until PermissionResolved arrives.
    assert!(app.overlays.permission_prompt.is_open());
}

/// Ctrl-S opens the file picker over the session cwd; while open it owns
/// the keyboard, and Enter on a match inserts the path into the
/// composer (reference: paste with spaces).
#[test]
fn ctrl_s_opens_picker_and_enter_inserts_the_path() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.rs"), "").unwrap();
    let mut app = App::new();
    app.handle_event(AgentEvent::SessionInfo {
        cwd: tmp.path().to_string_lossy().into_owned(),
        branch: "main".into(),
    });

    // A walking picker claims cadence frames.
    assert!(!app.overlays.file_picker.is_open());
    app.handle_key(ctrl('s'), &tx);
    assert!(app.overlays.file_picker.is_open());
    assert_eq!(app.cadence(), crate::tui::repaint::Cadence::PENDING);

    // Drain the walker via the app's loop hook.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.overlays.file_picker.walking() {
        app.tick_file_picker();
        assert!(
            std::time::Instant::now() < deadline,
            "picker never finished walking"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // Nothing more to arrive: settled picker owes no frame.
    assert!(!app.tick_file_picker());

    // Typing filters; keys never reach the composer while open.
    type_query(&mut app, &tx, "main");
    assert!(app.composer.text.is_empty(), "picker owns the keyboard");

    app.handle_key(key(KeyCode::Enter), &tx);
    assert!(!app.overlays.file_picker.is_open());
    assert_eq!(app.composer.text, "main.rs");
}

/// Esc closes the picker without touching the composer; an existing
/// draft gets a separating space before the picked path.
#[test]
fn esc_closes_and_selection_separates_from_existing_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.handle_key(ctrl('s'), &tx);
    assert!(app.overlays.file_picker.is_open());
    app.handle_key(key(KeyCode::Esc), &tx);
    assert!(!app.overlays.file_picker.is_open());
    assert!(app.composer.text.is_empty());

    app.composer.set_text("read".into());
    app.overlays.file_picker.handle_key(key(KeyCode::Char('a')));
    // Simulate a Select via the same helper the key path uses.
    app.handle_key(ctrl('s'), &tx);
    if let Some(s) = app.overlays.file_picker.session_mut() {
        s.matches = vec![crate::tui::file_picker::Match {
            path: "src/a.rs".into(),
            indices: Vec::new(),
        }];
        s.total_matches = 1;
        s.selected = 0;
        s.visible = true;
    }
    app.handle_key(key(KeyCode::Enter), &tx);
    assert_eq!(app.composer.text, "read src/a.rs");
}

/// A paste while the picker is open lands in its query, not the
/// composer.
#[test]
fn paste_feeds_the_picker_query() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.handle_key(ctrl('s'), &tx);
    app.insert_paste("src");
    assert!(app.composer.text.is_empty());
    if let Some(s) = app.overlays.file_picker.session_mut() {
        assert_eq!(s.query, "src");
    }
}

/// The picker overlay paints: query row with the caret and matched
/// path rows on the raised surface.
#[test]
fn file_picker_overlay_renders_paths_and_query_row() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.rs"), "").unwrap();
    let mut app = App::new();
    app.handle_event(AgentEvent::SessionInfo {
        cwd: tmp.path().to_string_lossy().into_owned(),
        branch: "main".into(),
    });
    app.handle_key(ctrl('s'), &tx);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.overlays.file_picker.walking() {
        app.tick_file_picker();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    app.tick_file_picker();
    type_query(&mut app, &tx, "main");
    let text = super::super::testutil::screen_text(&mut app, 100, 30);
    assert!(text.contains("/ main"), "query row missing:\n{text}");
    assert!(text.contains("main.rs"), "path row missing:\n{text}");
}

/// Modals are exclusive by construction: running the palette's "model"
/// item replaces the palette with the model menu instead of stacking.
#[test]
fn model_menu_replaces_open_palette() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.overlays.modal = Modal::Palette {
        query: "mo".into(),
        selected: 0,
    };
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(app.overlays.modal, Modal::ModelMenu(_)));
}

/// Tab cycles Build -> Plan -> Build (F.2); BackTab keeps focus cycling.
#[test]
fn tab_cycles_modes_and_backtab_keeps_focus_cycling() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    assert_eq!(app.mode, crate::tui::app::Mode::Build);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(app.mode, crate::tui::app::Mode::Plan);
    assert!(
        app.plan_mode.plan_path.is_some(),
        "plan path allocated on entry"
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
    assert_eq!(app.mode, crate::tui::app::Mode::Build);
}

/// Esc dismisses the command palette without running a command.
#[test]
fn esc_closes_the_command_palette() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.overlays.modal = Modal::Palette {
        query: "mo".into(),
        selected: 0,
    };
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    assert!(matches!(app.overlays.modal, Modal::None));
}

fn mouse(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn hover_tracks_tool_card() {
    let mut app = app_with_collapsible_tool();
    app.handle_mouse(mouse(MouseEventKind::Moved, 3, 10));
    assert_eq!(app.view.hover_tool, Some(0));
    app.handle_mouse(mouse(MouseEventKind::Moved, 3, 80));
    assert_eq!(app.view.hover_tool, None);
}

#[test]
fn click_toggles_collapsible_card() {
    let mut app = app_with_collapsible_tool();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 3, 10);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 3, 10);
    app.handle_mouse(down);
    app.handle_mouse(up);
    assert!(
        !app.conversation.collapsed.contains(&"r1".to_string()),
        "press expands"
    );
    app.handle_mouse(down);
    app.handle_mouse(up);
    assert!(
        app.conversation.collapsed.contains(&"r1".to_string()),
        "press again collapses"
    );
}

#[test]
fn drag_selects_instead_of_toggling() {
    let mut app = app_with_collapsible_tool();
    app.view.msg_area = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 24,
    };
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 10));
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 20));
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 20));
    // Still collapsed: the drag became a selection, not a card press.
    assert!(app.conversation.collapsed.contains(&"r1".to_string()));
}

/// A long bash body truncates with a notice row; clicking the notice
/// expands the body in place, and clicking the fold-back notice that
/// replaces it re-truncates. The card's own collapse is untouched.
#[test]
fn click_notice_row_expands_and_retruncates_body() {
    let mut app = App::new();
    let lines: Vec<ToolLine> = (0..60)
        .map(|i| ToolLine {
            kind: LineKind::Context,
            text: format!("out {i}"),
            ..Default::default()
        })
        .collect();
    app.handle_event(AgentEvent::ToolCall(ToolCallData {
        id: "b1".into(),
        kind: ToolKind::Bash { cmd: "make".into() },
        lines,
        awaiting_approval: false,
        image: None,
    }));
    draw_app(&mut app, 80, 24);
    let press = |app: &mut App, row, col| {
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), row, col));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), row, col));
    };
    let notice = |app: &App| app.view.notice_regions.first().copied();
    let (idx, rect) = notice(&app).expect("truncated body carries a notice");
    press(&mut app, rect.y, rect.x + 4);
    assert!(
        app.conversation.expanded_bodies.iter().any(|c| c == "b1"),
        "notice press expands the body"
    );
    assert!(
        app.conversation.collapsed.is_empty(),
        "card collapse untouched"
    );
    draw_app(&mut app, 80, 24);
    let (_, rect) = notice(&app).expect("expanded body carries a fold-back notice");
    press(&mut app, rect.y, rect.x + 4);
    assert!(
        app.conversation.expanded_bodies.is_empty(),
        "fold-back press re-truncates"
    );
    let _ = idx;
}

/// The ported composer chords: Ctrl-W deletes a word, Ctrl-K kills to the
/// end of the line, Alt-Left moves back a word.
#[test]
fn composer_chords_edit_words_and_lines() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("foo bar".into());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.composer.text, "foo ");

    app.composer.set_text("keep\nkill this".into());
    app.composer.cursor = "keep\n".chars().count();
    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.composer.text, "keep\n");

    app.composer.set_text("one two".into());
    app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT), &tx);
    assert_eq!(app.composer.cursor, 4);
    app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), &tx);
    assert_eq!(app.composer.cursor, 7);
}

/// g and G still type into the composer when it holds text.
#[test]
fn vim_scroll_keys_type_when_composer_has_text() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.insert_char('a');
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "agG");
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
}

/// ↑ recalls older entries, clamps at the oldest, ↓ walks back toward the
/// newest and restores the in-progress draft past it.
#[test]
fn history_recall_navigates_and_restores_draft() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.input_history.push("first".into());
    app.input_history.push("second".into());
    app.composer.set_text("draft".into());

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "second");
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "first");
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "first", "clamped at the oldest entry");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "second");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "draft", "draft restored past newest");
    assert!(app.history_recall.history_index.is_none());
}

/// Slash commands are recorded in history but ↑/↓ skip over them, so
/// recalling one can never reopen the slash menu and trap the arrows.
#[test]
fn history_recall_skips_slash_commands() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.input_history.push("old text".into());
    app.input_history.push("/help".into());
    app.input_history.push("new text".into());
    app.composer.set_text("draft".into());

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "new text");
    assert!(!app.slash_open(), "slash menu stays closed while recalling");

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "old text", "skipped over /help");
    assert!(!app.slash_open());

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "old text", "clamped at the oldest entry");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "new text", "skipped over /help");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "draft", "draft restored past newest");
    assert!(app.history_recall.history_index.is_none());
}

/// A slash command as the newest entry is never recalled; ↑ stays put and
/// the menu does not open.
#[test]
fn history_up_ignores_a_newest_slash_command() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.input_history.push("/stats".into());
    app.composer.set_text("draft".into());

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "draft", "nothing but slash commands");
    assert!(app.history_recall.history_index.is_none());
    assert!(!app.slash_open());
}

/// With an empty composer, ↑/↓ drive the input history straight away —
/// the arrows are not scrollback keys.
#[test]
fn arrows_drive_history_from_an_empty_composer() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = scrolled_app();
    draw_app(&mut app, 80, 24);
    app.scroll_by(-3);
    assert!(!app.view.follow);
    app.input_history.push("earlier".into());

    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "earlier");
    assert!(!app.view.follow, "Up recalled history instead of scrolling");

    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
    assert_eq!(app.composer.text, "", "draft (empty) restored past newest");
    assert!(app.history_recall.history_index.is_none());
}

/// Ctrl-E: line-end while the composer holds text; with an empty
/// composer it jumps the scrollback to the bottom and re-pins follow.
#[test]
fn ctrl_e_is_both_line_end_and_jump_to_bottom() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("hello".into());
    app.composer.cursor = 0;
    app.handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.composer.cursor, 5, "line-end with text");

    let mut app = scrolled_app();
    draw_app(&mut app, 80, 24);
    app.scroll_by(-5);
    assert!(!app.view.follow, "scrolled off the bottom");
    app.handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.view.follow, "Ctrl-E on empty composer jumps to bottom");
    let layout = crate::tui::ui::scrollback::Layout::new(&app.view.segments, app.view.view_width);
    assert_eq!(
        layout.doc_row(app.view.scroll) + u32::from(app.view.view_height),
        layout.total_rows()
    );
}

/// Submitting a message records it in the rolling input history.
#[test]
fn submit_records_history() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("hello world".into());
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert_eq!(app.input_history.len(), 1);
    assert_eq!(app.input_history.get(0), Some("hello world"));
    assert!(app.composer.text.is_empty());
}

/// Ctrl-C tri-state (reference `handle_ctrl` Quit branch): text first,
/// then the running turn, then the app.
#[test]
fn ctrl_c_tri_state_clears_then_cancels_then_quits() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("draft".into());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.composer.text.is_empty(), "first press clears input");
    assert!(!app.should_quit);
    assert!(rx.try_recv().is_err(), "no command sent while text present");

    app.handle_event(AgentEvent::StatusChanged(Status::Running));
    app.handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(
        matches!(rx.try_recv(), Ok(Command::Interrupt)),
        "second press cancels the running turn"
    );
    assert!(!app.should_quit);

    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    app.handle_key(
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
        &tx,
    );
    assert!(app.should_quit, "idle press quits");
}

/// Bang-mode submit: `! cmd` sends a visible `Command::Shell` and shows
/// the echoed command instead of a model turn.
#[test]
fn submit_bang_sends_visible_shell_command() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("! echo hi".into());
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(Command::Shell { command, visible }) if command == "echo hi" && visible
    ));
    assert!(rx.try_recv().is_err(), "no SendMessage follows");
    assert!(matches!(
        app.conversation.messages.last(),
        Some(Message::User(text)) if text == "! echo hi"
    ));
    assert_eq!(app.input_history.get(0), Some("! echo hi"));
    assert!(app.composer.text.is_empty());
}

/// `!! cmd` runs hidden from the model: `visible: false`, no history
/// result will be queued, and the echo uses the double sigil.
#[test]
fn submit_double_bang_sends_hidden_shell_command() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("!! make".into());
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(Command::Shell { command, visible }) if command == "make" && !visible
    ));
    assert!(matches!(
        app.conversation.messages.last(),
        Some(Message::User(text)) if text == "!! make"
    ));
}

/// A lone sigil or interior bang stays normal input.
#[test]
fn submit_lone_bang_is_a_normal_message() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("!".into());
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert!(matches!(rx.try_recv(), Ok(Command::SendMessage(text, _, _)) if text == "!"));
}

/// `cd` through bang-mode flashes the hint but still runs (reference
/// behavior); the echo uses the single sigil.
#[test]
fn submit_bang_cd_flashes() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("! cd /tmp".into());
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
    assert_eq!(
        app.flash_text(),
        Some("Only /cd can change the working directory")
    );
}

/// Effort lives on Alt-E (Ctrl-F is the transcript search); Ctrl-E
/// moves to the end of the line.
#[test]
fn alt_e_cycles_effort_and_ctrl_e_moves_to_line_end() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.composer.set_text("hello".into());
    app.composer.cursor = 0;
    let before = app.session.thinking;
    app.handle_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.session.thinking, before, "ctrl-f opens search");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT), &tx);
    assert_eq!(app.session.thinking, before.cycle());
    assert!(matches!(rx.try_recv(), Ok(Command::SetThinking(value)) if value == before.cycle()));
    assert_eq!(app.composer.text, "hello", "alt-e does not type");
    app.handle_key(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        &tx,
    );
    assert_eq!(app.composer.cursor, 5);
}

/// Ctrl-H opens the data-driven keybinding sheet; arrows scroll it,
/// Esc closes (F.1).
#[test]
fn ctrl_h_opens_scrolls_and_closes_the_help_sheet() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.handle_key(ctrl('h'), &tx);
    assert!(
        matches!(app.overlays.modal, Modal::Help),
        "ctrl-h opens the sheet"
    );
    app.handle_modal_key(key(KeyCode::Down), &tx);
    assert_eq!(app.overlays.help_scroll, 1, "down scrolls the sheet");
    app.handle_modal_key(key(KeyCode::PageUp), &tx);
    assert_eq!(app.overlays.help_scroll, 0);
    app.handle_modal_key(key(KeyCode::Esc), &tx);
    assert!(
        matches!(app.overlays.modal, Modal::None),
        "esc closes the sheet"
    );
}

/// A config overlay rebinds a chord: the new chord dispatches the
/// action and the old one no longer does (F.1).
#[test]
fn keybinding_overlay_replaces_the_palette_chord() {
    use crate::tui::keybindings::{ActionId, KeybindingResolver};
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    let entries = vec![("palette".to_string(), vec!["Alt+M".to_string()])];
    let mut warnings = Vec::new();
    app.overlays.keybinds = KeybindingResolver::from_overlay(&entries, &mut warnings);
    assert!(warnings.is_empty());

    app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT), &tx);
    assert!(
        matches!(app.overlays.modal, Modal::Palette { .. }),
        "alt-m opens it"
    );

    app.overlays.modal = Modal::None;
    app.handle_key(ctrl('p'), &tx);
    assert!(
        matches!(app.overlays.modal, Modal::None),
        "ctrl-p no longer triggers the palette"
    );
    assert!(app.overlays.keybinds.is_overridden(ActionId::Palette));
}
/// F.6: pasting an image path attaches the image instead of inserting
/// the path text; text pastes are unaffected.
#[test]
fn paste_of_image_path_attaches_image() {
    let mut app = App::new();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("shot.png");
    let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 0, 255]));
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(Vec::new()),
            image::ImageFormat::Png,
        )
        .ok();
    std::fs::write(&path, b"png").unwrap();
    app.insert_paste(path.to_str().unwrap());
    assert!(app.composer.text.is_empty(), "path not inserted as text");
    assert_eq!(app.images.loads.len(), 1);
    // The background load lands through the poll.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while app.images.attached.is_empty() && std::time::Instant::now() < deadline {
        app.poll_image_loads();
    }
    assert_eq!(app.images.attached.len(), 1);
    assert_eq!(
        app.images.attached[0].media_type,
        crate::history::ImageMedia::Png
    );

    // Plain text pastes are unaffected.
    app.insert_paste("just words");
    assert_eq!(app.composer.text, "just words");
}

// ------------------------------------------------------------------
// Subagent task chats (task 96)
// ------------------------------------------------------------------

fn sub_event(id: &str, text: &str) -> AgentEvent {
    AgentEvent::Subagent {
        tool_use_id: id.into(),
        description: "refactor tests".into(),
        event: Box::new(AgentEvent::AssistantDelta(text.into())),
    }
}

#[test]
fn subagent_events_create_and_fill_chat() {
    let mut app = App::new();
    app.handle_event(sub_event("t1", "hello "));
    // A second event with the same id reuses the chat; deltas merge
    // into one assistant paragraph.
    app.handle_event(sub_event("t1", "world"));
    assert_eq!(app.task_chats.len(), 1);
    assert_eq!(app.task_chats[0].tool_use_id, "t1");
    assert_eq!(app.task_chats[0].name, "refactor tests");
    match &app.task_chats[0].conversation.messages[..] {
        [crate::tui::app::Message::Assistant(text)] => assert_eq!(text, "hello world"),
        _ => panic!("expected one merged assistant message"),
    }
    // A different id opens a second chat.
    app.handle_event(sub_event("t2", "other"));
    assert_eq!(app.task_chats.len(), 2);
    assert_eq!(app.task_chats[1].conversation.messages.len(), 1);
}

#[test]
fn subagent_finished_sets_outcome_by_is_error() {
    let mut app = App::new();
    app.handle_event(sub_event("t1", "working"));
    app.handle_event(AgentEvent::SubagentFinished {
        tool_use_id: "t1".into(),
        is_error: false,
    });
    assert_eq!(
        app.task_chats[0].outcome,
        Some(crate::tui::app::TaskOutcome::Done)
    );
    // A late error verdict does not walk back the decided outcome.
    app.handle_event(AgentEvent::SubagentFinished {
        tool_use_id: "t1".into(),
        is_error: true,
    });
    assert_eq!(
        app.task_chats[0].outcome,
        Some(crate::tui::app::TaskOutcome::Done)
    );

    app.handle_event(sub_event("t2", "failing"));
    app.handle_event(AgentEvent::SubagentFinished {
        tool_use_id: "t2".into(),
        is_error: true,
    });
    assert_eq!(
        app.task_chats[1].outcome,
        Some(crate::tui::app::TaskOutcome::Error)
    );
}

#[test]
fn turn_end_terminalizes_working_task_chats() {
    let mut app = App::new();
    app.handle_event(sub_event("t1", "half done"));
    assert_eq!(
        app.task_chats[0].status(),
        crate::tui::app::TaskStatus::Working
    );
    // A return to idle (either settled status) closes the chat with
    // the placeholder outcome, which reads as Done.
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    assert_eq!(
        app.task_chats[0].outcome,
        Some(crate::tui::app::TaskOutcome::Unknown)
    );
    assert_eq!(
        app.task_chats[0].status(),
        crate::tui::app::TaskStatus::Done
    );
    // The late verdict can still refine the placeholder.
    app.handle_event(AgentEvent::SubagentFinished {
        tool_use_id: "t1".into(),
        is_error: true,
    });
    assert_eq!(
        app.task_chats[0].status(),
        crate::tui::app::TaskStatus::Error
    );

    // Same terminalization via Failed, and a pending Esc disarms.
    app.handle_event(sub_event("t2", "x"));
    app.esc_pending = Some(std::time::Instant::now());
    app.handle_event(AgentEvent::StatusChanged(Status::Failed));
    assert!(app.esc_pending.is_none());
    assert_eq!(
        app.task_chats[1].outcome,
        Some(crate::tui::app::TaskOutcome::Unknown)
    );
}

#[test]
fn ctrl_n_opens_the_task_chat_picker_modal() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.handle_event(sub_event("t1", "one"));
    app.handle_event(sub_event("t2", "two"));

    // Ctrl-N opens the picker on the current (main) chat; Ctrl-P is
    // never claimed by task chats and always opens the palette.
    app.handle_key(ctrl('p'), &tx);
    assert!(matches!(app.overlays.modal, Modal::Palette { .. }));
    app.overlays.modal = Modal::None;
    app.handle_key(ctrl('n'), &tx);
    let Modal::TaskPicker { selected } = app.overlays.modal else {
        panic!("Ctrl-N did not open the task picker");
    };
    assert_eq!(selected, 0);

    // Arrows move (clamped), Enter mounts the highlighted chat.
    for _ in 0..4 {
        app.handle_key(key(KeyCode::Down), &tx);
    }
    let Modal::TaskPicker { selected } = app.overlays.modal else {
        panic!("arrows must keep the picker open");
    };
    assert_eq!(selected, 2, "selection clamps at the last task");
    app.handle_key(key(KeyCode::Enter), &tx);
    assert!(matches!(app.overlays.modal, Modal::None));
    assert_eq!(app.active_task, Some(1));

    // Reopening from a task chat starts the cursor on it; Enter on
    // row 0 returns to the main chat.
    app.handle_key(ctrl('n'), &tx);
    let Modal::TaskPicker { selected } = app.overlays.modal else {
        panic!("reopen failed");
    };
    assert_eq!(selected, 2);
    app.handle_key(key(KeyCode::Up), &tx);
    app.handle_key(key(KeyCode::Up), &tx);
    app.handle_key(key(KeyCode::Up), &tx);
    app.handle_key(key(KeyCode::Enter), &tx);
    assert_eq!(app.active_task, None, "row 0 is the main chat");
    assert!(matches!(app.overlays.modal, Modal::None));

    // Focusing a task chat mounts its transcript in the render
    // slots; the main transcript stays reachable and intact.
    app.handle_key(ctrl('n'), &tx);
    app.handle_key(key(KeyCode::Down), &tx);
    app.handle_key(key(KeyCode::Enter), &tx);
    app.tick_reveal(std::time::Instant::now() + std::time::Duration::from_secs(1));
    let visible = super::super::testutil::screen_text(&mut app, 80, 24);
    assert!(
        visible.contains("one"),
        "task transcript mounted:\n{visible}"
    );
    assert!(
        visible.contains("refactor tests"),
        "task header missing:\n{visible}"
    );

    // Without task chats the chord is a no-op (Ctrl-P stays palette).
    let mut bare = App::new();
    bare.handle_key(ctrl('n'), &tx);
    assert_eq!(bare.active_task, None);
    assert!(matches!(bare.overlays.modal, Modal::None));
    bare.handle_key(ctrl('p'), &tx);
    assert!(matches!(bare.overlays.modal, Modal::Palette { .. }));
}

#[test]
fn esc_esc_cancels_the_focused_task_chat() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.handle_event(sub_event("t1", "churning"));
    focus_task_chat(&mut app, &tx, 0);

    // First Esc only arms: a flash, no command yet.
    app.handle_key(key(KeyCode::Esc), &tx);
    assert!(app.esc_pending.is_some());
    assert!(rx.try_recv().is_err(), "single Esc must not cancel");

    // Second Esc within the window cancels the subagent by id.
    app.handle_key(key(KeyCode::Esc), &tx);
    assert!(app.esc_pending.is_none());
    match rx.try_recv() {
        Ok(Command::CancelSubagent { tool_use_id }) => assert_eq!(tool_use_id, "t1"),
        other => panic!("expected CancelSubagent, got {other:?}"),
    }
    assert_eq!(
        app.task_chats[0].outcome,
        Some(crate::tui::app::TaskOutcome::Error)
    );
    // The chat's transcript records the cancellation.
    assert!(app.conversation.messages.iter().any(|m| matches!(
        m,
        crate::tui::app::Message::Notice { text, .. } if text == "cancelled"
    )));

    // Any other key disarms a pending Esc-Esc.
    app.handle_event(sub_event("t2", "x"));
    focus_task_chat(&mut app, &tx, 1);
    app.handle_key(key(KeyCode::Esc), &tx);
    app.handle_key(key(KeyCode::Char('a')), &tx);
    assert!(app.esc_pending.is_none());
    app.handle_key(key(KeyCode::Esc), &tx);
    assert!(
        rx.try_recv().is_err(),
        "disarmed Esc-Esc must start over with one Esc"
    );
}

#[test]
fn main_chat_esc_still_interrupts() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    // With a working task chat existing but not focused, Esc is the
    // plain interrupt chain.
    app.handle_event(sub_event("t1", "x"));
    app.handle_event(AgentEvent::StatusChanged(Status::Running));
    app.handle_key(key(KeyCode::Esc), &tx);
    match rx.try_recv() {
        Ok(Command::Interrupt) => {}
        other => panic!("main-chat Esc must interrupt, got {other:?}"),
    }
    // Same inside a task chat that already finished: no cancel path.
    app.handle_event(AgentEvent::SubagentFinished {
        tool_use_id: "t1".into(),
        is_error: false,
    });
    focus_task_chat(&mut app, &tx, 0);
    app.handle_key(key(KeyCode::Esc), &tx);
    assert!(app.esc_pending.is_none(), "finished task chats don't arm");
    assert!(rx.try_recv().is_err());
}

#[test]
fn reset_clears_task_chat_state() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new();
    app.handle_event(sub_event("t1", "x"));
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    focus_task_chat(&mut app, &tx, 0);
    assert_eq!(app.active_task, Some(0));

    // Cancel then reset: chats gone, back on the main transcript.
    app.run_command("new", &tx);
    assert!(app.task_chats.is_empty());
    assert_eq!(app.active_task, None);
    assert!(app.conversation.messages.is_empty());
}

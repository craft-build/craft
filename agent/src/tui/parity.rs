//! Golden-fixture scenes for the paint path.
//!
//! A fixed set of deterministic apps (fixed cwd/branch/model strings and
//! transcript content) rendered through the slt paint path by
//! [`crate::tui::parity_slt`], which either writes (`UPDATE_FIXTURES=1`)
//! or compares against the checked-in fixture files in
//! `agent/fixtures/golden/`.

#![cfg(test)]

use std::path::PathBuf;

use crate::tui::app::App;
use crate::tui::provider::{AgentEvent, Status, ToolCallData, ToolKind};

/// Terminal size for every scene.
pub(crate) const W: u16 = 80;
pub(crate) const H: u16 = 24;

/// Rows of a rendered `slt` buffer, trailing blanks trimmed. Wide-char
/// continuation cells are normalized to a space so the dumps compare
/// byte-identically with the historical fixture captures (ratatui wrote
/// " " into skipped cells; slt leaves the symbol empty — visually
/// identical).
pub(crate) fn buffer_rows(buf: &slt::buffer::Buffer) -> String {
    (0..buf.area.height)
        .map(|r| {
            (0..buf.area.width)
                .map(|c| {
                    let s = buf.get(c, r).symbol.to_string();
                    if s.is_empty() { " ".to_string() } else { s }
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// OSC-8 card links resolve repo paths against this checkout; normalize
/// them so fixtures are machine-independent.
pub(crate) fn normalize(frame: String) -> String {
    frame.replace(concat!(env!("CARGO_MANIFEST_DIR"), "/"), "<ROOT>/")
}

pub(crate) fn seeded_app() -> App {
    let mut app = App::new();
    // Fixed chrome strings so fixtures never depend on machine state.
    app.session.cwd = "/craft".into();
    app.session.branch = "main".into();
    app
}

/// A settled turn: user prompt, streaming assistant reply, tool call, done.
fn scene_transcript() -> App {
    let mut app = seeded_app();
    app.handle_event(AgentEvent::SessionLoaded {
        messages: vec![crate::tui::provider::LoadedMessage::User(
            "please read src/x.ts and explain".into(),
        )],
        draft: String::new(),
        mode: None,
    });
    app.handle_event(AgentEvent::AssistantText(
        "Reading the file first, then I'll explain what it does. `src/x.ts` \
         exports the widget."
            .into(),
    ));
    app.handle_event(AgentEvent::ToolCall(ToolCallData {
        awaiting_approval: false,
        id: "r1".into(),
        kind: ToolKind::Read {
            path: "src/x.ts".into(),
            summary: "10 lines".into(),
        },
        lines: vec![],
        image: None,
    }));
    app.handle_event(AgentEvent::AssistantText(
        "The file defines a `Widget` class with `render` and a default \
         export. 中文注释 confirms it wraps the DOM node — CJK width matters \
         for wrapping parity."
            .into(),
    ));
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    app
}

/// Long transcript scrolled to the middle (document engine + viewport math).
fn scene_scrolled() -> App {
    let mut app = seeded_app();
    for i in 0..30 {
        app.handle_event(AgentEvent::AssistantText(format!(
            "message number {i} with some wrapping text to fill rows"
        )));
        app.handle_event(AgentEvent::AssistantDelta(" …".into()));
    }
    app.handle_event(AgentEvent::AssistantEnd);
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    // Scrolled up half a screen: exercise the mid-viewport clamp math.
    app.scroll_by(8);
    app
}

/// Composer mid-edit: prompt text past wrapping, including CJK + bang-mode.
fn scene_composer() -> App {
    let mut app = seeded_app();
    app.composer.set_text(
        "! git status && git diff --stat  zweite Zeile 中文输入测试 wrappable tail".into(),
    );
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    app
}

/// Permission prompt open (spoofed caret rows inside the prompt box).
fn scene_permission() -> App {
    let mut app = seeded_app();
    app.handle_event(AgentEvent::StatusChanged(Status::WaitingApproval));
    app.overlays.permission_prompt.open(
        "pr-req-1".into(),
        "Bash".into(),
        vec!["command:rm".into()],
        vec!["/tmp/scratch".into()],
        vec!["rm -rf /tmp/scratch && echo done".into()],
    );
    app
}

/// Command palette open with a filtered query.
fn scene_palette() -> App {
    let mut app = seeded_app();
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    app.overlays.modal = crate::tui::modals::Modal::Palette {
        query: "mo".into(),
        selected: 0,
    };
    app
}

/// Help overlay.
fn scene_help() -> App {
    let mut app = seeded_app();
    app.handle_event(AgentEvent::StatusChanged(Status::Done));
    app.overlays.modal = crate::tui::modals::Modal::Help;
    app
}

/// Question form with a free-text field.
fn scene_question() -> App {
    use crate::tools::QuestionOption;
    let mut app = seeded_app();
    app.overlays.question_form.open(
        "q-1".to_string(),
        vec![crate::tools::QuestionSpec {
            question: "Which database should the app target?".into(),
            header: None,
            options: vec![
                QuestionOption {
                    label: "SQLite".into(),
                    description: None,
                },
                QuestionOption {
                    label: "Postgres".into(),
                    description: None,
                },
            ],
            multi_select: false,
        }],
    );
    app
}

pub(crate) fn scenes() -> Vec<(&'static str, App)> {
    vec![
        ("empty", seeded_app()),
        ("transcript", scene_transcript()),
        ("scrolled", scene_scrolled()),
        ("composer", scene_composer()),
        ("permission", scene_permission()),
        ("palette", scene_palette()),
        ("help", scene_help()),
        ("question_direct", scene_question()),
    ]
}

pub(crate) fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/golden")
        .join(format!("{name}.txt"))
}

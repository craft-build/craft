//! Golden-fixture parity harness for the ratatui → superlighttui migration.
//!
//! Renders a fixed set of scenes through the current paint path and either
//! writes (`UPDATE_FIXTURES=1`) or compares against the checked-in fixture
//! files in `agent/fixtures/golden/`. The scenes are deterministic: sessions
//! are seeded with fixed cwd/branch/model strings and transcript content.
//!
//! Wave 1 (type swap) fixtures are captured from the ratatui paint path
//! through the compat shims; the paint-layer wave re-captures by rendering
//! the same scenes through slt, and this file is where the render backend
//! call lives, so only this module changes at that point.

#![cfg(test)]

use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::tui::app::App;
use crate::tui::provider::{AgentEvent, Status, ToolCallData, ToolKind};
use crate::tui::ui;

/// Terminal size for every scene.
const W: u16 = 80;
const H: u16 = 24;

fn render_screen(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();
    terminal.draw(|f| ui::draw(f, app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let frame = (0..buf.area.height)
        .map(|r| {
            (0..buf.area.width)
                .map(|c| buf[(c, r)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    // OSC-8 card links resolve repo paths against this checkout; normalize
    // them so fixtures are machine-independent.
    frame.replace(concat!(env!("CARGO_MANIFEST_DIR"), "/"), "<ROOT>/")
}

fn seeded_app() -> App {
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

fn scenes() -> Vec<(&'static str, App)> {
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

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/golden")
        .join(format!("{name}.txt"))
}

#[test]
fn golden_fixture_parity() {
    let update = std::env::var("UPDATE_FIXTURES").is_ok_and(|v| v == "1");
    if update {
        std::fs::create_dir_all(fixture_path("empty").parent().unwrap()).unwrap();
    }
    let mut failures = Vec::new();
    for (name, mut app) in scenes() {
        let frame = render_screen(&mut app);
        let path = fixture_path(name);
        if update {
            std::fs::write(&path, &frame).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("missing fixture {name} (run with UPDATE_FIXTURES=1 to capture): {e}")
        });
        if frame != expected {
            failures.push(format!(
                "scene `{name}` drifted from fixture {}\n--- fixture (first 5 lines)\n{}\n--- rendered (first 5 lines)\n{}",
                path.display(),
                expected.lines().take(5).collect::<Vec<_>>().join("\n"),
                frame.lines().take(5).collect::<Vec<_>>().join("\n"),
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

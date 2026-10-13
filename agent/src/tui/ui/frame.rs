//! The `slt` frame: root layout for the whole TUI, replacing the legacy
//! ratatui `draw()` composition one panel at a time (wave 2a).
//!
//! Layout mirrors `draw()` exactly — same regions, same theme colors, same
//! heights — verified against the golden fixtures by `tui::parity_slt`:
//!
//! ```text
//! root col (bg = bg_app)
//! ├─ row [ chat col (grow) | sidebar (w=34, when open) ]
//! │    chat col:
//! │    ├─ task header (1 row, task chats)
//! │    ├─ transcript container (grow)     ← messages painter (segment flow)
//! │    ├─ plan form / question form / permission prompt (stacked above
//! │    │   the composer block, in that order, exactly as before)
//! │    └─ bottom block (shared top border):
//! │        border row │ composer box │ status row │ footer hint row
//! └─ trailing full-screen draw: selection REVERSED + frame_text snapshot
//!    + modal overlays declared after the base UI
//! ```

use crate::tui::app::App;
use crate::tui::ui::theme;

use super::messages;
use super::{composer, sidebar};

/// Terminal width at which the sidebar may share the layout (legacy gate).
const SIDEBAR_MIN_WIDTH: u32 = 80;
/// Right-column width (legacy `SIDEBAR_WIDTH`).
const SIDEBAR_WIDTH: u32 = 34;

/// View state that persists across frames on the `slt` side. The document
/// engine in `App` stays the source of truth; the input-widget states here
/// are render mirrors (wave 2b) — the app owns every edit, the widgets own
/// caret rendering.
#[derive(Default)]
pub struct View {
    /// The in-TUI editor modal (Alt-O composer handoff / Ctrl-O plan
    /// editing), replacing the old `$EDITOR` spawn. Owned here so the
    /// frame closure stays `Send + 'static`.
    pub editor: Option<EditorForm>,
    /// Composer render mirror (wave 2b): fed the legacy `wrap_rows` layout
    /// each frame so row breaks stay byte-identical while the textarea
    /// paints the real caret (`▎` glyph + hardware cursor position).
    pub composer: slt::TextareaState,
}

/// A multiline edit session rendered as a modal over the whole UI.
///
/// Wave 2b: the buffer is a real `slt::widgets::TextareaState` — native
/// editing (grapheme-safe caret, word jumps, Ctrl-Z/Ctrl-Y undo/redo,
/// bracketed paste) with the `▎` glyph + hardware-cursor caret. The
/// widget owns editing while the modal is open; the run-loop closure
/// consumes everything except the widget's own edit keys (see
/// `editor_key_edits_widget`) plus Esc/Ctrl-S, which it intercepts.
pub struct EditorForm {
    /// Where the edited text goes on save.
    pub origin: EditorOrigin,
    /// The edit buffer; `wrap_width` stays `None` (one visual row per
    /// logical line, matching the 2a rows rendering).
    pub state: slt::TextareaState,
}

/// What a completed [`EditorForm`] save edits.
#[derive(Clone, Debug)]
pub enum EditorOrigin {
    /// Replace the composer text (Alt-O).
    ComposerDraft,
    /// Overwrite the plan file in place (Ctrl-O).
    PlanFile(std::path::PathBuf),
}

impl EditorForm {
    pub fn open(origin: EditorOrigin, text: &str) -> Self {
        let mut state = slt::TextareaState::new();
        state.set_value(text);
        // `set_value` parks the cursor at (0, 0); open at the end, where
        // the 2a form (and every editor) puts it.
        let last = state.lines.len() - 1;
        state.cursor_row = last;
        state.cursor_col = state.lines[last].chars().count();
        Self { origin, state }
    }

    pub fn text(&self) -> String {
        self.state.value()
    }
}

/// Full frame paint on the `slt` backend. Counterpart of the legacy
/// `ui::draw`; the two must produce identical output (see `parity_slt`).
pub fn frame(ui: &mut slt::Context, app: &mut App, view: &mut View) {
    app.status_tick = app.status_tick.wrapping_add(1);
    app.clear_expired_flash();

    let t = theme::current();
    let width = ui.width();

    // Root: one full-size column with the app background.
    let _ = ui.container().bg(t.bg_app).grow(1).col(|ui| {
        let show_sidebar = app.session.sidebar_open && width >= SIDEBAR_MIN_WIDTH;
        if show_sidebar {
            let _ = ui.container().grow(1).row(|ui| {
                chat_column(ui, app, view, width - SIDEBAR_WIDTH);
                sidebar::render_slt(ui, app);
            });
        } else {
            chat_column(ui, app, view, width);
        }

        // Overlays, back to front (modal stack, matching draw() ordering).
        super::overlays::render_slt(ui, app);

        // The in-TUI editor modal (Alt-O / Ctrl-O), full screen above all.
        if let Some(editor) = view.editor.as_mut() {
            render_editor_modal(ui, editor);
        }

        // Trailing pass over the finished buffer: current selection
        // highlight and the frame_text snapshot for the next event's
        // copy/region reads (reads run one frame late in both worlds).
        messages::overlay_pass_slt(ui, app);
    });
}

/// The chat half: transcript on top, forms stacked above the composer
/// block, all pinned to the bottom. `avail` is the chat column's width.
fn chat_column(ui: &mut slt::Context, app: &mut App, view: &mut View, avail: u32) {
    let _ = ui.container().grow(1).col(|ui| {
        // Task-chat header (task 96): one line above the transcript.
        if app.active_task.is_some() {
            render_task_header_slt(ui, app);
        }

        // Transcript: the segment-flow painter owns this region.
        messages::render_slt(ui, app);

        // Forms stack directly above the composer block, in draw() order:
        // plan form lowest (closest to the composer), then the question
        // form, then the permission prompt on top.
        crate::tui::permission_prompt::render_slt(ui, app);
        crate::tui::question_form::render_slt(ui, app);
        crate::tui::plan_form::render_slt(ui, app);

        bottom_block(ui, app, view, avail);
    });
}

/// The shared-top-border block: border row, composer box, status row, and
/// the blank footer hint row.
fn bottom_block(ui: &mut slt::Context, app: &App, view: &mut View, avail: u32) {
    let t = theme::current();
    let _ = ui
        .container()
        .border(slt::Border::Single)
        .border_sides(slt::style::BorderSides {
            top: true,
            ..slt::style::BorderSides::none()
        })
        .border_fg(t.border_subtle)
        .grow(0)
        .col(|ui| {
            composer::render_slt(ui, app, &mut view.composer, avail);
        });
}

/// One-line task header above a focused task chat (task 96).
fn render_task_header_slt(ui: &mut slt::Context, app: &App) {
    use crate::tui::style::{Color, Style};

    let t = theme::current();
    let Some(chat) = app.task_chats.get(app.active_task.unwrap_or(usize::MAX)) else {
        return;
    };
    let status = chat.status();
    let status_color = match status {
        crate::tui::app::TaskStatus::Working => t.warning,
        crate::tui::app::TaskStatus::Done => t.success,
        crate::tui::app::TaskStatus::Error => t.danger,
    };
    ui.line(|ui| {
        ui.styled(chat.name.clone(), Style::default().fg(t.accent));
        ui.text(" · ");
        ui.styled(status.label(), Style::default().fg(status_color));
        if status == crate::tui::app::TaskStatus::Working {
            ui.styled("  Esc-Esc cancels", Style::default().fg(t.text_tertiary));
        }
    });
    let _ = Color::Reset; // theme import hygiene for compile-time checks
}

/// Render the in-TUI editor modal: a rounded box over the whole screen
/// with a title, the textarea, and a save/cancel footer hint.
///
/// The textarea sits in a fixed-width wrapper (its rect then predicts
/// reliably) and a trailing overlay restyles the rows — the widget paints
/// with slt's theme, the editor wants the plain default look of the 2a
/// rows.
fn render_editor_modal(ui: &mut slt::Context, editor: &mut EditorForm) {
    let t = theme::current();
    let width = ui.width().max(20);
    let height = ui.height().max(8);
    let box_w = (width.saturating_sub(8)).min(78);
    let rows = editor.state.lines.len().min(24) as u32;
    let box_h = (rows + 3).min(height.saturating_sub(2));
    let inner_w = box_w.saturating_sub(2); // border insets

    let mut area_rect: Option<slt::Rect> = None;
    let _ = ui.modal(|ui| {
        let _ = ui
            .container()
            .bg(t.bg_overlay)
            .border(slt::Border::Rounded)
            .border_fg(t.border_strong)
            .ml((width.saturating_sub(box_w)) / 2)
            .mt((height.saturating_sub(box_h)) / 2)
            .w(box_w)
            .h(box_h)
            .col(|ui| {
                ui.text(" ✎ edit").bold().fg(t.text_secondary);
                let resp = ui.container().w(inner_w).py(0).col(|ui| {
                    let _ = ui.textarea(&mut editor.state, rows);
                });
                area_rect = Some(resp.rect);
                ui.line(|ui| {
                    ui.text("Ctrl-S save   Esc cancel   Ctrl-Z undo")
                        .fg(t.text_tertiary);
                });
            });
    });

    // Trailing restyle from the wrapper's previous-frame rect (the same
    // one-frame handoff cadence as the composer overlays).
    if let Some(rect) = area_rect.filter(|r| r.width > 0 && r.height > 0) {
        let _ = ui.overlay(|ui| {
            ui.container().grow(1).draw(move |buf, _area| {
                let x_end = rect.x.saturating_add(rect.width);
                let y_end = rect.y.saturating_add(rect.height);
                for y in rect.y..y_end {
                    for x in rect.x..x_end {
                        if let Some(cell) = buf.try_get_mut(x, y) {
                            cell.style = slt::Style::default();
                        }
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> EditorForm {
        EditorForm::open(EditorOrigin::ComposerDraft, text)
    }

    #[test]
    fn open_places_caret_at_end() {
        let e = editor("ab\ncd");
        let last = e.state.lines.len() - 1;
        assert_eq!(
            (e.state.cursor_row, e.state.cursor_col),
            (last, e.state.lines[last].chars().count())
        );
        assert_eq!(e.text(), "ab\ncd");
    }

    #[test]
    fn text_round_trips_through_the_state() {
        let mut e = editor("hello\nworld");
        e.state.set_value("replaced");
        assert_eq!(e.text(), "replaced");
        // set_value parks the caret at the start; open() is what moves it
        // to the end.
        assert_eq!((e.state.cursor_row, e.state.cursor_col), (0, 0));
    }

    #[test]
    fn empty_text_still_has_one_row() {
        let e = editor("");
        assert_eq!(e.state.lines, vec![String::new()]);
        assert_eq!((e.state.cursor_row, e.state.cursor_col), (0, 0));
    }
}

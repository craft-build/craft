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

/// View state that persists across frames on the `slt` side. Wave 2a keeps
/// it minimal (the document engine in `App` stays the source of truth);
/// wave 2b moves the editable fields onto slt input-widget states here.
#[derive(Default)]
pub struct View {
    /// The in-TUI editor modal (Alt-O composer handoff / Ctrl-O plan
    /// editing), replacing the old `$EDITOR` spawn. Owned here so the
    /// frame closure stays `Send + 'static`.
    pub editor: Option<EditorForm>,
    // wave 2b: composer `TextareaState`, palette/search/picker
    // `TextInputState`s live here for hardware-caret fidelity.
}

/// A multiline edit session rendered as a modal over the whole UI.
///
/// The 2a editor form is `TextBuffer`-backed (a plain multi-row buffer with
/// a spoofed caret, identical to the query rows) — wave 2b swaps the buffer
/// for `slt::widgets::TextareaState` for a real caret and native undo.
pub struct EditorForm {
    /// Where the edited text goes on save.
    pub origin: EditorOrigin,
    /// Rows of the buffer (lines split on `\n`; the trailing newline of a
    /// nonempty buffer expresses the trailing empty row).
    pub rows: Vec<String>,
    /// Char row of the caret.
    pub row: usize,
    /// Char column of the caret within `rows[row]`.
    pub col: usize,
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
        let mut rows: Vec<String> = text.split('\n').map(str::to_owned).collect();
        if rows.is_empty() {
            rows.push(String::new());
        }
        let row = rows.len() - 1;
        let col = rows[row].chars().count();
        Self {
            origin,
            rows,
            row,
            col,
        }
    }

    pub fn text(&self) -> String {
        self.rows.join("\n")
    }

    fn byte_index(&self) -> usize {
        self.rows[self.row]
            .char_indices()
            .nth(self.col)
            .map_or(self.rows[self.row].len(), |(i, _)| i)
    }

    pub fn insert_char(&mut self, c: char) {
        let at = self.byte_index();
        self.rows[self.row].insert(at, c);
        self.col += 1;
    }

    pub fn insert_newline(&mut self) {
        let at = self.byte_index();
        let rest = self.rows[self.row].split_off(at);
        self.rows.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let back = self.last_char_len();
            let at = self.byte_index();
            self.rows[self.row].remove(at - back);
            self.col -= 1;
        } else if self.row > 0 {
            let joined = self.rows.remove(self.row);
            self.row -= 1;
            self.col = self.rows[self.row].chars().count();
            self.rows[self.row].push_str(&joined);
        }
    }

    pub fn delete(&mut self) {
        if self.col < self.rows[self.row].chars().count() {
            let at = self.byte_index();
            self.rows[self.row].remove(at);
        } else if self.row + 1 < self.rows.len() {
            let joined = self.rows.remove(self.row + 1);
            self.rows[self.row].push_str(&joined);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.rows[self.row].chars().count();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.rows[self.row].chars().count() {
            self.col += 1;
        } else if self.row + 1 < self.rows.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.rows[self.row].chars().count());
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.rows.len() {
            self.row += 1;
            self.col = self.col.min(self.rows[self.row].chars().count());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.rows[self.row].chars().count();
    }

    /// Byte length of the char just left of the caret (for backspace).
    fn last_char_len(&self) -> usize {
        self.rows[self.row]
            .chars()
            .nth(self.col.saturating_sub(1))
            .map_or(1, char::len_utf8)
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
                chat_column(ui, app, width - SIDEBAR_WIDTH);
                sidebar::render_slt(ui, app);
            });
        } else {
            chat_column(ui, app, width);
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
fn chat_column(ui: &mut slt::Context, app: &mut App, avail: u32) {
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

        bottom_block(ui, app, avail);
    });
}

/// The shared-top-border block: border row, composer box, status row, and
/// the blank footer hint row.
fn bottom_block(ui: &mut slt::Context, app: &mut App, avail: u32) {
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
            composer::render_slt(ui, app, avail);
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
/// with a title, the buffer rows, and a save/cancel footer hint.
fn render_editor_modal(ui: &mut slt::Context, editor: &mut EditorForm) {
    let t = theme::current();
    let width = ui.width().max(20);
    let height = ui.height().max(8);
    let box_w = (width.saturating_sub(8)).min(78);
    let rows = editor.rows.len().min(24) as u32;
    let box_h = (rows + 3).min(height.saturating_sub(2));

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
                let _ = ui.container().grow(1).py(0).col(|ui| {
                    editor_rows(ui, editor);
                });
                ui.line(|ui| {
                    ui.text("Ctrl-S save   Esc cancel").fg(t.text_tertiary);
                });
                ui.styled(
                    "in-TUI editor (wave 2a): $EDITOR handoff returns with wave 2b",
                    slt::Style::default().fg(t.text_disabled),
                );
            });
    });
}

/// The editor buffer rows, informationally styled; wave 2a renders a
/// spoofed caret (reversed cell), wave 2b hands this to `TextareaState`.
fn editor_rows(ui: &mut slt::Context, editor: &EditorForm) {
    use crate::tui::style::{Modifier, Style, StyleExt};

    let t = theme::current();
    let caret_style = Style::default().add_modifier(Modifier::REVERSED);
    for (r, row) in editor.rows.iter().enumerate() {
        ui.line(|ui| {
            if r == editor.row {
                let prefix: String = row.chars().take(editor.col).collect();
                let caret_ch: String = row
                    .chars()
                    .nth(editor.col)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| " ".to_string());
                let suffix: String = row.chars().skip(editor.col + 1).collect();
                if !prefix.is_empty() {
                    ui.styled(prefix, slt::Style::default());
                }
                ui.styled(&caret_ch, caret_style);
                if !suffix.is_empty() {
                    ui.styled(suffix, slt::Style::default());
                }
            } else {
                ui.styled(row.clone(), slt::Style::default());
            }
            let _ = t.bg_app; // keep theme linkage explicit for the 2b restyle
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
        assert_eq!((e.row, e.col), (1, 2));
        assert_eq!(e.text(), "ab\ncd");
    }

    #[test]
    fn edits_split_and_join_lines() {
        let mut e = editor("hello");
        e.left();
        e.insert_newline();
        assert_eq!(e.text(), "hell\no");
        assert_eq!((e.row, e.col), (1, 0));
        e.backspace(); // join back
        assert_eq!(e.text(), "hello");
        assert_eq!((e.row, e.col), (0, 4));
    }

    #[test]
    fn backspace_eats_multibyte_chars_whole() {
        let mut e = editor("你");
        e.backspace();
        assert_eq!(e.text(), "");
        assert_eq!((e.row, e.col), (0, 0));
    }

    #[test]
    fn vertical_moves_clamp_to_line_lengths() {
        let mut e = editor("abc\n中");
        e.home();
        e.right();
        e.right();
        e.down(); // col 2 clamps to 1 (one CJK char)
        assert_eq!((e.row, e.col), (1, 1));
        e.up();
        assert_eq!((e.row, e.col), (0, 1));
    }

    #[test]
    fn delete_merges_following_line() {
        let mut e = editor("ab\ncd");
        e.up(); // caret at end of "ab" (col clamps to 2)
        e.delete();
        assert_eq!(e.text(), "abcd");
    }
}

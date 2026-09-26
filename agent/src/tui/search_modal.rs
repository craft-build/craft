//! Fuzzy message search (F.3, Ctrl-F): a nucleo fuzzy matcher over the
//! transcript's rendered segments, with jump-to-segment and next/prev.
//! Ported from the reference `craft-ui/src/components/search_modal.rs`,
//! adapted to this repo: the corpus is this repo's segment cache and the
//! modal's render lives with the other overlays (`ui::overlays`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::tui::ui::scrollback::ScrollPos;

pub(crate) const MODAL_TITLE: &str = " Search ";
const SEARCH_ROW: u16 = 1;
const SEARCH_PREFIX: &str = "/ ";
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";

pub(crate) struct SearchMatch {
    pub segment_index: usize,
    /// Row within the segment carrying the first matched char.
    pub display_row: usize,
    pub score: u16,
    pub display_indices: Vec<u32>,
    pub display_line: String,
}

pub enum SearchAction {
    Consumed,
    Navigate,
    Select(usize, usize),
    Close(Option<(ScrollPos, bool)>),
}

pub struct SearchModal {
    query: String,
    /// Cursor position in chars from the start of `query`.
    cursor: usize,
    pub(crate) matches: Vec<SearchMatch>,
    pub(crate) selected: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) viewport_height: usize,
    open: bool,
    saved_scroll: Option<(ScrollPos, bool)>,
    matcher: Matcher,
}

impl Default for SearchModal {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchModal {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            open: false,
            saved_scroll: None,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn open(&mut self, scroll: ScrollPos, follow: bool) {
        self.reset();
        self.open = true;
        self.saved_scroll = Some((scroll, follow));
    }

    pub fn close(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.open = false;
        self.query.clear();
        self.cursor = 0;
        self.matches.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.saved_scroll = None;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_paste(&mut self, text: &str) {
        let byte = self.byte_of_cursor();
        self.query.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SearchAction {
        match key.code {
            KeyCode::Esc => SearchAction::Close(self.saved_scroll.take()),
            KeyCode::Enter => {
                if let Some(m) = self.matches.get(self.selected) {
                    SearchAction::Select(m.segment_index, m.display_row)
                } else {
                    SearchAction::Close(self.saved_scroll.take())
                }
            }
            KeyCode::Up => {
                self.move_up();
                SearchAction::Navigate
            }
            KeyCode::Down => {
                self.move_down();
                SearchAction::Navigate
            }
            _ => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('w') | KeyCode::Backspace)
                {
                    self.delete_word_back();
                } else {
                    self.edit(key);
                }
                SearchAction::Consumed
            }
        }
    }

    fn move_up(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1);
            self.ensure_visible();
        }
    }

    fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
            self.ensure_visible();
        }
    }

    pub(crate) fn ensure_visible(&mut self) {
        if self.viewport_height == 0 {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + self.viewport_height {
            self.scroll_offset = self.selected + 1 - self.viewport_height;
        }
    }

    // --- minimal single-line text editing (reference uses TextBuffer) ---

    fn edit(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL) => {
                let byte = self.byte_of_cursor();
                self.query.insert(byte, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let byte = self.byte_of_cursor();
                    let end = next_char_boundary(&self.query, byte);
                    self.query.drain(byte..end);
                }
            }
            KeyCode::Delete => {
                let byte = self.byte_of_cursor();
                if byte < self.query.len() {
                    let end = next_char_boundary(&self.query, byte);
                    self.query.drain(byte..end);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.query.chars().count() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.chars().count(),
            _ => {}
        }
    }

    fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.query.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.query = chars[..i].iter().collect::<String>()
            + &chars[self.cursor..].iter().collect::<String>();
        self.cursor = i;
    }

    fn byte_of_cursor(&self) -> usize {
        self.query
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.query.len())
    }

    /// `corpus` is called only once there is something to match, so an empty
    /// query never pays to materialize the transcript.
    pub fn update_matches(&mut self, corpus: impl FnOnce() -> Vec<String>) {
        self.matches.clear();
        self.selected = 0;
        self.scroll_offset = 0;

        if self.query.trim().is_empty() {
            return;
        }

        let atom = Atom::new(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );

        let mut buf = Vec::new();
        let mut indices = Vec::new();
        for (idx, text) in corpus().iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            buf.clear();
            indices.clear();
            let haystack = Utf32Str::new(text, &mut buf);
            if let Some(score) = atom.indices(haystack, &mut self.matcher, &mut indices) {
                let (display_line, display_row, display_indices) =
                    pick_display_line(text, &indices);
                self.matches.push(SearchMatch {
                    segment_index: idx,
                    display_row,
                    score,
                    display_indices,
                    display_line,
                });
            }
        }

        self.matches.sort_by_key(|m| std::cmp::Reverse(m.score));
    }

    pub fn current_segment_index(&self) -> Option<(usize, usize)> {
        self.matches
            .get(self.selected)
            .map(|m| (m.segment_index, m.display_row))
    }

    /// Test accessor: number of current matches.
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }
}

fn next_char_boundary(s: &str, byte: usize) -> usize {
    s[byte..]
        .char_indices()
        .nth(1)
        .map(|(b, _)| byte + b)
        .unwrap_or(s.len())
}

/// The one display line of a multi-line segment that carries the first
/// matched char, with the match indices remapped into that line.
fn pick_display_line(text: &str, indices: &[u32]) -> (String, usize, Vec<u32>) {
    let first_idx = indices.iter().copied().min().unwrap_or(0);
    let mut char_offset = 0u32;
    for (row, line) in text.lines().enumerate() {
        let line_char_count = line.chars().count() as u32;
        if first_idx < char_offset + line_char_count {
            let remapped: Vec<u32> = indices
                .iter()
                .filter(|&&i| i >= char_offset && i < char_offset + line_char_count)
                .map(|&i| i - char_offset)
                .collect();
            return (line.to_string(), row, remapped);
        }
        char_offset += line_char_count + 1;
    }
    let first_line = text.lines().next().unwrap_or("").to_string();
    (first_line, 0, Vec::new())
}

/// One result row: the matched chars bolded in the accent color, the
/// selected row on the overlay background.
pub(crate) fn highlighted_row(
    m: &SearchMatch,
    max_width: usize,
    is_selected: bool,
) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Span;

    let t = crate::tui::ui::theme::current();
    let index_set: std::collections::HashSet<u32> = m.display_indices.iter().copied().collect();
    let base_style = Style::default().bg(if is_selected {
        t.bg_overlay
    } else {
        t.bg_raised
    });
    let match_style = base_style.fg(t.accent).add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(LABEL_INDENT, base_style)];
    let mut current_highlighted = false;
    let mut run = String::new();
    for (char_pos, ch) in m.display_line.chars().enumerate().take(max_width) {
        let is_match = index_set.contains(&(char_pos as u32));
        if is_match != current_highlighted && !run.is_empty() {
            let style = if current_highlighted {
                match_style
            } else {
                base_style
            };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        current_highlighted = is_match;
        run.push(ch);
    }
    if !run.is_empty() {
        let style = if current_highlighted {
            match_style
        } else {
            base_style
        };
        spans.push(Span::styled(run, style));
    }
    spans
}

pub(crate) fn no_matches_label() -> &'static str {
    NO_MATCHES
}

pub(crate) fn search_prefix() -> &'static str {
    SEARCH_PREFIX
}

pub(crate) fn search_row_height() -> u16 {
    SEARCH_ROW
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn modal_with_query(query: &str, texts: &[&str]) -> SearchModal {
        let mut modal = SearchModal::new();
        modal.open(ScrollPos::default(), true);
        modal.query = query.to_string();
        modal.cursor = query.chars().count();
        modal.update_matches(|| texts.iter().map(|t| (*t).to_owned()).collect());
        modal
    }

    #[test]
    fn matching_finds_correct_segments() {
        let modal = modal_with_query("hello", &["hello world", "foo bar", "say hello"]);
        assert_eq!(modal.matches.len(), 2);
        assert!(modal.matches.iter().all(|m| !m.display_indices.is_empty()));
        let seg: Vec<usize> = modal.matches.iter().map(|m| m.segment_index).collect();
        assert!(seg.contains(&0));
        assert!(seg.contains(&2));
    }

    #[test]
    fn matches_sorted_by_score_descending() {
        let modal = modal_with_query("fb", &["foobar", "fb", "f---b"]);
        assert!(modal.matches.len() >= 2);
        for w in modal.matches.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn navigation_wraps_around() {
        let mut modal = modal_with_query("item", &["item a", "item b", "item c"]);
        assert_eq!(modal.selected, 0);

        modal.handle_key(key_event(KeyCode::Down));
        assert_eq!(modal.selected, 1);
        modal.handle_key(key_event(KeyCode::Down));
        assert_eq!(modal.selected, 2);
        modal.handle_key(key_event(KeyCode::Down));
        assert_eq!(modal.selected, 0);

        modal.handle_key(key_event(KeyCode::Up));
        assert_eq!(modal.selected, 2);
    }

    #[test]
    fn enter_selects_current_match() {
        let mut modal = modal_with_query("hello", &["hello world", "foo bar", "say hello"]);
        modal.handle_key(key_event(KeyCode::Down));
        let expected_seg = modal.matches[1].segment_index;
        match modal.handle_key(key_event(KeyCode::Enter)) {
            SearchAction::Select(idx, _) => assert_eq!(idx, expected_seg),
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn enter_on_no_matches_closes() {
        let mut modal = modal_with_query("zzz", &["hello", "world"]);
        assert!(matches!(
            modal.handle_key(key_event(KeyCode::Enter)),
            SearchAction::Close(_)
        ));
    }

    #[test]
    fn close_clears_state() {
        let mut modal = modal_with_query("hello", &["hello world"]);
        assert!(!modal.matches.is_empty());
        modal.close();
        assert!(modal.matches.is_empty());
        assert!(modal.query.is_empty());
        assert!(!modal.is_open());
    }

    #[test]
    fn close_restores_the_saved_scroll() {
        let mut modal = SearchModal::new();
        let saved = (ScrollPos { seg: 3, row: 1 }, false);
        modal.open(saved.0, saved.1);
        match modal.handle_key(key_event(KeyCode::Esc)) {
            SearchAction::Close(got) => assert_eq!(got, Some(saved)),
            _ => panic!("expected Close"),
        }
    }

    #[test]
    fn editing_moves_the_cursor_and_multibyte_survives() {
        let mut modal = SearchModal::new();
        modal.open(ScrollPos::default(), true);
        modal.handle_key(key_event(KeyCode::Char('a')));
        modal.handle_key(key_event(KeyCode::Char('b')));
        modal.handle_key(key_event(KeyCode::Left));
        modal.handle_key(key_event(KeyCode::Char('é')));
        assert_eq!(modal.query(), "aéb");
        modal.handle_key(key_event(KeyCode::Backspace));
        assert_eq!(modal.query(), "ab");
        assert_eq!(modal.cursor(), 1);
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_cursor() {
        let mut modal = SearchModal::new();
        modal.open(ScrollPos::default(), true);
        modal.query = "foo bar".into();
        modal.cursor = 7;
        let mut key = key_event(KeyCode::Char('w'));
        key.modifiers = KeyModifiers::CONTROL;
        modal.handle_key(key);
        assert_eq!(modal.query(), "foo ");
        assert_eq!(modal.cursor(), 4);
    }

    #[test]
    fn display_line_picks_matched_line() {
        let modal = modal_with_query("second", &["header\nsecond line\nthird"]);
        assert_eq!(modal.matches.len(), 1);
        assert_eq!(modal.matches[0].display_line, "second line");
    }

    #[test]
    fn search_role_prefix_matches() {
        let modal = modal_with_query("craft>", &["you> hello", "craft> world"]);
        assert_eq!(modal.matches.len(), 1);
        assert_eq!(modal.matches[0].segment_index, 1);
    }
}

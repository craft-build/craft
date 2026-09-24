//! Composer input editing: text + cursor manipulation shared by key
//! handling and bracketed paste.

/// The message composer: its text and cursor (a char index into `text`).
pub struct Composer {
    pub text: String,
    pub cursor: usize,
}

impl Composer {
    pub fn new() -> Self {
        Composer {
            text: String::new(),
            cursor: 0,
        }
    }

    /// Byte index of the `char_idx`-th char.
    fn byte_index(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    pub fn insert_char(&mut self, c: char) {
        let idx = self.byte_index(self.cursor);
        self.text.insert(idx, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let idx = self.byte_index(self.cursor - 1);
            self.text.remove(idx);
            self.cursor -= 1;
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    fn chars(&self) -> Vec<char> {
        self.text.chars().collect()
    }

    /// Char index of the previous word boundary (start of the word before
    /// `x`), skipping whitespace backwards first — read/readline semantics.
    fn find_prev_word_boundary(&self, x: usize) -> usize {
        let chars = self.chars();
        let mut i = x;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn find_next_word_boundary(&self, x: usize) -> usize {
        let chars = self.chars();
        let len = chars.len();
        let mut i = x;
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Ctrl-W / Ctrl-Backspace: delete back to the previous word boundary.
    pub fn delete_word_back(&mut self) {
        let new_cursor = self.find_prev_word_boundary(self.cursor);
        let (start, end) = (self.byte_index(new_cursor), self.byte_index(self.cursor));
        self.text.replace_range(start..end, "");
        self.cursor = new_cursor;
    }

    /// Ctrl-Delete: delete forward to the next word boundary.
    pub fn delete_word_forward(&mut self) {
        let end = self.find_next_word_boundary(self.cursor);
        let (start, stop) = (self.byte_index(self.cursor), self.byte_index(end));
        self.text.replace_range(start..stop, "");
    }

    /// Ctrl-K: kill from the cursor to the end of the current line.
    pub fn kill_to_end_of_line(&mut self) {
        let chars = self.chars();
        let line_end = chars[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|off| self.cursor + off)
            .unwrap_or(chars.len());
        let (start, stop) = (self.byte_index(self.cursor), self.byte_index(line_end));
        self.text.replace_range(start..stop, "");
    }

    /// Alt/Ctrl-Left: move back one word.
    pub fn move_word_left(&mut self) {
        self.cursor = self.find_prev_word_boundary(self.cursor);
    }

    /// Alt/Ctrl-Right: move forward one word.
    pub fn move_word_right(&mut self) {
        self.cursor = self.find_next_word_boundary(self.cursor);
    }

    pub fn move_home(&mut self) {
        let chars = self.chars();
        let line_start = chars[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = line_start;
    }

    pub fn move_end(&mut self) {
        let chars = self.chars();
        let line_end = chars[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|off| self.cursor + off)
            .unwrap_or(chars.len());
        self.cursor = line_end;
    }

    pub fn move_up(&mut self) {
        let chars = self.chars();
        let line_start = chars[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if line_start == 0 {
            return; // already on the first line
        }
        let col = self.cursor - line_start;
        let prev_start = chars[..line_start - 1]
            .iter()
            .rposition(|&c| c == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = (prev_start + col).min(line_start - 1);
    }

    pub fn move_down(&mut self) {
        let chars = self.chars();
        let line_end = chars[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map(|off| self.cursor + off)
            .unwrap_or(chars.len());
        if line_end == chars.len() {
            return; // already on the last line
        }
        let col = self.cursor
            - chars[..self.cursor]
                .iter()
                .rposition(|&c| c == '\n')
                .map(|i| i + 1)
                .unwrap_or(0);
        let next_end = chars[line_end + 1..]
            .iter()
            .position(|&c| c == '\n')
            .map(|off| line_end + 1 + off)
            .unwrap_or(chars.len());
        self.cursor = (line_end + 1 + col).min(next_end);
    }

    /// True when the cursor sits on the first (logical) line, so Up should
    /// navigate history instead of moving within the text.
    pub fn cursor_on_first_line(&self) -> bool {
        !self.text[..self.byte_index(self.cursor)].contains('\n')
    }

    /// True when the cursor sits on the last (logical) line, so Down should
    /// navigate history instead of moving within the text.
    pub fn cursor_on_last_line(&self) -> bool {
        !self.text[self.byte_index(self.cursor)..].contains('\n')
    }

    pub fn set_text(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Insert bracketed-paste content as one unit — crucially it never
    /// triggers submit (raw newlines would have arrived as Enter keypresses
    /// and sent the message line by line). Newlines are kept: the composer
    /// wraps and renders multi-line input.
    pub fn insert_paste(&mut self, text: &str) {
        // Bracketed paste delivers line breaks as \r or \r\n depending on the
        // terminal; normalize both to \n.
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if clean.trim().is_empty() {
            return;
        }
        let chars_added = clean.chars().count();
        let idx = self.byte_index(self.cursor);
        self.text.insert_str(idx, &clean);
        self.cursor += chars_added;
    }
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_preserves_line_breaks_and_moves_cursor() {
        let mut composer = Composer::new();
        // Multi-line paste with both \r\n and \r line endings (how bracketed
        // paste can deliver breaks) lands as \n-separated text in one unit.
        composer.insert_paste("one\r\ntwo\rthree");
        assert_eq!(composer.text, "one\ntwo\nthree");
        assert_eq!(composer.cursor, composer.text.chars().count());
    }

    #[test]
    fn editing_moves_the_cursor() {
        let mut composer = Composer::new();
        composer.insert_char('a');
        composer.insert_char('b');
        composer.insert_char('c');
        composer.move_left();
        composer.backspace(); // delete 'b'
        assert_eq!(composer.text, "ac");
        assert_eq!(composer.cursor, 1);
        composer.move_right();
        composer.move_right(); // clamps at the end
        assert_eq!(composer.cursor, 2);
        composer.clear();
        assert_eq!(composer.text, "");
        assert_eq!(composer.cursor, 0);
    }

    #[test]
    fn word_motions_and_edits() {
        let mut composer = Composer::new();
        composer.set_text("foo bar baz".into()); // cursor at the end
        composer.move_word_left();
        assert_eq!(composer.cursor, 8, "start of 'baz'");
        composer.delete_word_back(); // ctrl-w removes 'bar '
        assert_eq!(composer.text, "foo baz");
        assert_eq!(composer.cursor, 4);
        composer.delete_word_back(); // removes 'foo '
        assert_eq!(composer.text, "baz");
        assert_eq!(composer.cursor, 0);
    }

    #[test]
    fn ctrl_w_skips_leading_whitespace() {
        let mut composer = Composer::new();
        composer.set_text("a b   ".into());
        composer.delete_word_back();
        assert_eq!(composer.text, "a ");
    }

    #[test]
    fn kill_to_end_of_line_stops_at_newline() {
        let mut composer = Composer::new();
        composer.set_text("first line\nsecond".into());
        composer.cursor = 0;
        composer.kill_to_end_of_line();
        assert_eq!(composer.text, "\nsecond");
        assert_eq!(composer.cursor, 0);
    }

    #[test]
    fn delete_word_forward_cuts_to_the_next_boundary() {
        let mut composer = Composer::new();
        composer.set_text("one two".into());
        composer.move_word_left(); // to 4
        composer.delete_word_forward();
        assert_eq!(composer.text, "one ");
    }

    #[test]
    fn home_end_and_line_motions() {
        let mut composer = Composer::new();
        composer.set_text("alpha\nbeta\ngamma".into());
        assert!(composer.cursor_on_last_line());
        assert!(!composer.cursor_on_first_line());
        composer.move_up();
        assert_eq!(composer.cursor, "alpha\nbeta".chars().count());
        composer.move_home();
        assert_eq!(composer.cursor, "alpha\n".chars().count());
        composer.move_up();
        assert_eq!(composer.cursor, 0);
        assert!(composer.cursor_on_first_line());
        composer.move_end();
        assert_eq!(composer.cursor, "alpha".chars().count());
        composer.move_down(); // column 5 clamps to the end of 'beta'
        assert_eq!(composer.cursor, "alpha\nbeta".chars().count());
        composer.move_down(); // column carries over to 'gamma'
        assert_eq!(composer.cursor, "alpha\nbeta\ngamm".chars().count());
        composer.move_down(); // already on the last line: no movement
        assert_eq!(composer.cursor, "alpha\nbeta\ngamm".chars().count());
        assert!(composer.cursor_on_last_line());
    }

    #[test]
    fn line_motions_preserve_column() {
        let mut composer = Composer::new();
        composer.set_text("alpha\nb\ngamma".into()); // cursor at end of 'gamma'
        composer.move_up();
        assert_eq!(composer.cursor, "alpha\nb".chars().count()); // col clamped
        composer.move_up(); // col 1 carries to 'alpha'
        assert_eq!(composer.cursor, 1);
    }
}

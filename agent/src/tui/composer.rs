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
}

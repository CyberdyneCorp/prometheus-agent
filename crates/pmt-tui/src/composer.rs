//! The composer: the multi-line text being typed, with its cursor.

use unicode_width::UnicodeWidthStr as _;

/// The text being typed. The cursor is a byte offset on a character
/// boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
}

impl Composer {
    /// The text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Nothing typed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text, the cursor at its end.
    pub fn set(&mut self, text: &str) {
        text.clone_into(&mut self.text);
        self.cursor = self.text.len();
    }

    /// Takes the text out, leaving the composer empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Types `c` at the cursor.
    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Types `text` at the cursor (a paste: line breaks kept, `\r\n` and
    /// `\r` normalised).
    pub fn insert_str(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    /// Deletes the character before the cursor.
    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    /// Deletes the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    /// Moves one character left.
    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    /// Moves one character right.
    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// Moves to the start of the cursor's line.
    pub fn line_start(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |at| at + 1);
    }

    /// Moves to the end of the cursor's line.
    pub fn line_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |at| self.cursor + at);
    }

    /// The cursor's line and display column.
    #[must_use]
    pub fn cursor_position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let row = before.matches('\n').count();
        let column = before.rsplit('\n').next().unwrap_or_default().width();
        (row, column)
    }

    /// The word ending at the cursor (completion's prefix) and where it
    /// starts.
    #[must_use]
    pub fn word_at_cursor(&self) -> (usize, &str) {
        let before = &self.text[..self.cursor];
        let start = before
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |at| at + 1);
        (start, &before[start..])
    }

    /// Replaces the text from `start` to the cursor with `with`.
    pub fn replace_word(&mut self, start: usize, with: &str) {
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
    }
}

#[cfg(test)]
mod tests {
    use super::Composer;

    #[test]
    fn editing_keeps_the_cursor_on_character_boundaries() {
        let mut c = Composer::default();
        for ch in "héllo".chars() {
            c.insert(ch);
        }
        c.left();
        c.left();
        c.backspace();
        assert_eq!(c.text(), "hélo");
        c.line_start();
        c.delete();
        assert_eq!(c.text(), "élo");
        c.line_end();
        c.insert_str("\r\nworld");
        assert_eq!(c.text(), "élo\nworld");
        assert_eq!(c.cursor_position(), (1, 5));
        assert_eq!(c.take(), "élo\nworld");
        assert!(c.is_empty());
    }

    #[test]
    fn the_word_at_the_cursor_is_replaced_by_a_completion() {
        let mut c = Composer::default();
        c.set("look at @./sr");
        let (start, word) = c.word_at_cursor();
        assert_eq!(word, "@./sr");
        c.replace_word(start, "@./src/");
        assert_eq!(c.text(), "look at @./src/");
    }
}

//! The composer: the multi-line text being typed, with its cursor, and
//! the atoms in it (spec: tui, Large pastes collapse into chips; Clipboard
//! image attachments): a long paste shows as a chip `[Pasted text #<n>:
//! <lines> lines]` and an attached image as a badge `[Image #<n>]`, each
//! deleted as a unit; a chip can be expanded back into its text, and the
//! submitted prompt carries every chip's full text and every badge's
//! image.

use std::path::PathBuf;

use unicode_width::UnicodeWidthStr as _;

/// The text being typed. The cursor is a byte offset on a character
/// boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
    atoms: Vec<Atom>,
    pastes: usize,
    images: usize,
}

/// A chip or a badge: its label in the text and what it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Atom {
    label: String,
    kind: AtomKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AtomKind {
    /// A collapsed paste's text.
    Paste(String),
    /// An attached image's file.
    Image(PathBuf),
}

/// When a paste collapses into a chip (`ui.composer`; `0` disables a
/// threshold).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collapse {
    /// At least this many lines.
    pub lines: u32,
    /// At least this many characters.
    pub chars: u32,
}

impl Collapse {
    fn applies(self, text: &str) -> bool {
        let lines = text.lines().count();
        let chars = text.chars().count();
        (self.lines > 0 && lines >= self.lines as usize)
            || (self.chars > 0 && chars >= self.chars as usize)
    }
}

/// What a submitted composer holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Submission {
    /// The prompt, every chip replaced by its text and badges removed.
    pub text: String,
    /// The attached images, in order.
    pub images: Vec<PathBuf>,
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

    /// Replaces the text (dropping any chip or badge), the cursor at its
    /// end.
    pub fn set(&mut self, text: &str) {
        self.clear_atoms();
        text.clone_into(&mut self.text);
        self.cursor = self.text.len();
    }

    /// Takes the text out (chips and badges as their labels), leaving the
    /// composer empty.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.clear_atoms();
        text
    }

    /// Takes the prompt out: chips expanded, badges turned into images.
    pub fn submit(&mut self) -> Submission {
        let mut text = std::mem::take(&mut self.text);
        let mut images = Vec::new();
        for atom in std::mem::take(&mut self.atoms) {
            let Some(at) = text.find(&atom.label) else {
                continue;
            };
            match atom.kind {
                AtomKind::Paste(pasted) => text.replace_range(at..at + atom.label.len(), &pasted),
                AtomKind::Image(path) => {
                    let end = at + atom.label.len();
                    // The badge and one space after it.
                    let end = if text[end..].starts_with(' ') {
                        end + 1
                    } else {
                        end
                    };
                    text.replace_range(at..end, "");
                    images.push(path);
                }
            }
        }
        self.clear_atoms();
        Submission {
            text: text.trim_end().to_owned(),
            images,
        }
    }

    fn clear_atoms(&mut self) {
        self.cursor = 0;
        self.atoms.clear();
        self.pastes = 0;
        self.images = 0;
    }

    /// A paste: collapsed into a chip when `collapse` says so, else typed.
    pub fn paste(&mut self, text: &str, collapse: Collapse, strings: &pmt_i18n::Strings) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if !collapse.applies(&text) {
            self.insert_str(&text);
            return;
        }
        self.pastes += 1;
        let lines = text.lines().count();
        let label = pmt_i18n::t!(strings, "tui.pasted", 0 = self.pastes, 1 = lines);
        self.insert_atom(label, AtomKind::Paste(text));
    }

    /// Attaches the image at `path` as a badge at the cursor; returns its
    /// label.
    pub fn attach_image(&mut self, path: PathBuf, strings: &pmt_i18n::Strings) -> String {
        self.images += 1;
        let label = pmt_i18n::t!(strings, "tui.image", 0 = self.images);
        self.insert_atom(label.clone(), AtomKind::Image(path));
        label
    }

    fn insert_atom(&mut self, label: String, kind: AtomKind) {
        let spaced = if self.text[..self.cursor].ends_with([' ', '\n']) || self.cursor == 0 {
            format!("{label} ")
        } else {
            format!(" {label} ")
        };
        self.text.insert_str(self.cursor, &spaced);
        self.cursor += spaced.len();
        self.atoms.push(Atom { label, kind });
    }

    /// The attached images.
    #[must_use]
    pub fn images(&self) -> usize {
        self.atoms
            .iter()
            .filter(|a| matches!(a.kind, AtomKind::Image(_)))
            .count()
    }

    /// Expands the nearest chip before the cursor into its text; `false`
    /// when there is none.
    pub fn expand(&mut self) -> bool {
        let before = &self.text[..self.cursor];
        let found = self
            .atoms
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a.kind, AtomKind::Paste(_)))
            .filter_map(|(n, a)| before.rfind(&a.label).map(|at| (at, n)))
            .max();
        let Some((at, n)) = found else {
            return false;
        };
        let atom = self.atoms.remove(n);
        let AtomKind::Paste(pasted) = atom.kind else {
            return false;
        };
        self.text.replace_range(at..at + atom.label.len(), &pasted);
        self.cursor = at + pasted.len();
        true
    }

    /// The atom whose label ends at the cursor (`before`) or starts there.
    fn atom_at_cursor(&self, before: bool) -> Option<(usize, usize)> {
        self.atoms.iter().enumerate().find_map(|(n, atom)| {
            let len = atom.label.len();
            let at = if before {
                self.cursor.checked_sub(len)?
            } else {
                self.cursor
            };
            (self.text.get(at..at + len) == Some(atom.label.as_str())).then_some((n, at))
        })
    }

    fn remove_atom(&mut self, n: usize, at: usize) {
        let atom = self.atoms.remove(n);
        self.text.replace_range(at..at + atom.label.len(), "");
        self.cursor = at;
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

    /// Deletes the character before the cursor, or the whole chip or
    /// badge that ends there.
    pub fn backspace(&mut self) {
        if let Some((n, at)) = self.atom_at_cursor(true) {
            self.remove_atom(n, at);
            return;
        }
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    /// Deletes the character under the cursor, or the whole chip or badge
    /// that starts there.
    pub fn delete(&mut self) {
        if let Some((n, at)) = self.atom_at_cursor(false) {
            self.remove_atom(n, at);
            return;
        }
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
    use std::path::PathBuf;

    use super::{Collapse, Composer};

    const COLLAPSE: Collapse = Collapse {
        lines: 5,
        chars: 2000,
    };

    /// Scenario: Long log paste.
    #[test]
    fn a_long_paste_is_one_chip_and_the_submission_has_every_line() {
        let log = (1..=300)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut c = Composer::default();
        c.set("look at this:");
        c.paste(
            &log,
            COLLAPSE,
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        assert_eq!(c.text(), "look at this: [Pasted text #1: 300 lines] ");
        c.insert_str("why?");
        let sent = c.submit();
        let logged = sent.text.lines().filter(|l| l.contains("line ")).count();
        assert_eq!(logged, 300);
        assert!(
            sent.text.starts_with("look at this: line 1\n"),
            "{}",
            &sent.text[..30]
        );
        assert!(sent.text.ends_with("line 300\n why?"), "{}", sent.text);
        assert!(c.is_empty());
    }

    #[test]
    fn short_pastes_are_typed_and_zero_disables_a_threshold() {
        let mut c = Composer::default();
        c.paste(
            "two\nlines",
            COLLAPSE,
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        assert_eq!(c.text(), "two\nlines");
        let mut c = Composer::default();
        let off = Collapse { lines: 0, chars: 0 };
        c.paste(
            &"x\n".repeat(50),
            off,
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        assert!(!c.text().contains("Pasted"));
        let mut c = Composer::default();
        c.paste(
            &"y".repeat(2000),
            COLLAPSE,
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        assert_eq!(c.text(), "[Pasted text #1: 1 lines] ");
    }

    #[test]
    fn chips_and_badges_delete_as_a_unit_and_chips_expand() {
        let mut c = Composer::default();
        c.paste(
            &"a\n".repeat(6),
            COLLAPSE,
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        c.backspace();
        c.backspace();
        assert_eq!(c.text(), "", "the space, then the whole chip");
        c.paste(
            &"a\n".repeat(6),
            COLLAPSE,
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        assert!(c.expand());
        assert_eq!(c.text(), "a\n".repeat(6) + " ");
        let label = c.attach_image(
            PathBuf::from("/tmp/x.png"),
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        assert_eq!(label, "[Image #1]");
        assert_eq!(c.images(), 1);
        c.backspace();
        c.backspace();
        assert_eq!(c.images(), 0);
        c.attach_image(
            PathBuf::from("/tmp/y.png"),
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        c.insert_str("what is it");
        let sent = c.submit();
        assert_eq!(sent.images, [PathBuf::from("/tmp/y.png")]);
        assert!(!sent.text.contains("[Image"), "{}", sent.text);
    }

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

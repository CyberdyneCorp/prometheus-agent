//! Key chords of `ui.tui.keys` (spec: tui, Configurable key bindings): the
//! grammar is `[Ctrl+][Alt+][Shift+]<key>`, modifiers in any order and any
//! case, where `<key>` is one printable character, `F1`-`F12` or a named
//! key (`Enter`, `Esc`, `Tab`, `Backspace`, `Delete`, `Insert`, `Home`,
//! `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`, `Space`).
//! Configuration validation refuses any other text, naming the key.

use std::fmt;

/// A key without modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A printable character (letters lower-cased).
    Char(char),
    /// A function key, 1 to 12.
    F(u8),
    /// Enter / Return.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Insert.
    Insert,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
}

const NAMED: &[(&str, Key)] = &[
    ("enter", Key::Enter),
    ("return", Key::Enter),
    ("esc", Key::Esc),
    ("escape", Key::Esc),
    ("tab", Key::Tab),
    ("backspace", Key::Backspace),
    ("delete", Key::Delete),
    ("del", Key::Delete),
    ("insert", Key::Insert),
    ("home", Key::Home),
    ("end", Key::End),
    ("pageup", Key::PageUp),
    ("pagedown", Key::PageDown),
    ("up", Key::Up),
    ("down", Key::Down),
    ("left", Key::Left),
    ("right", Key::Right),
    ("space", Key::Char(' ')),
];

/// A key with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    /// Ctrl held.
    pub ctrl: bool,
    /// Alt (Option) held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
    /// The key.
    pub key: Key,
}

impl Chord {
    /// A chord of `key` without modifiers.
    #[must_use]
    pub const fn plain(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            key,
        }
    }

    /// `Ctrl+<key>`.
    #[must_use]
    pub const fn ctrl(key: Key) -> Self {
        Self {
            ctrl: true,
            ..Self::plain(key)
        }
    }

    /// `Alt+<key>`.
    #[must_use]
    pub const fn alt(key: Key) -> Self {
        Self {
            alt: true,
            ..Self::plain(key)
        }
    }

    /// Parses `Ctrl+Alt+P`, `PageUp`, `Shift+Enter`, `F2`...
    ///
    /// # Errors
    ///
    /// A one-line reason naming the part that is not a modifier or key.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut chord = Self::plain(Key::Enter);
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        // `Ctrl++` binds the plus key itself.
        let (modifiers, key) = match parts.as_slice() {
            [rest @ .., "", ""] if !rest.is_empty() => (rest, "+"),
            [rest @ .., key] => (rest, *key),
            [] => (&[][..], ""),
        };
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "alt" | "option" | "meta" => chord.alt = true,
                "shift" => chord.shift = true,
                other => return Err(format!("'{other}' is not a modifier (Ctrl, Alt, Shift)")),
            }
        }
        chord.key = key_named(key).ok_or_else(|| {
            format!("'{key}' is not a key (a character, F1-F12, Enter, Esc, Tab, PageUp, ...)")
        })?;
        Ok(chord)
    }
}

fn key_named(name: &str) -> Option<Key> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return (c.is_ascii_graphic()).then(|| Key::Char(c.to_ascii_lowercase()));
    }
    let lower = name.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=12).contains(&n).then_some(Key::F(n));
    }
    NAMED.iter().find(|(n, _)| *n == lower).map(|(_, key)| *key)
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
        ] {
            if on {
                f.write_str(name)?;
            }
        }
        match self.key {
            Key::Char(' ') => f.write_str("Space"),
            Key::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            Key::F(n) => write!(f, "F{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Chord, Key};

    #[test]
    fn chords_parse_in_any_case_and_order_and_print_canonically() {
        let chord = Chord::parse("shift+ctrl+p").unwrap();
        assert!(chord.ctrl && chord.shift && !chord.alt);
        assert_eq!(chord.key, Key::Char('p'));
        assert_eq!(chord.to_string(), "Ctrl+Shift+P");
        assert_eq!(Chord::parse("PageUp").unwrap(), Chord::plain(Key::PageUp));
        assert_eq!(Chord::parse("Alt+Enter").unwrap(), Chord::alt(Key::Enter));
        assert_eq!(Chord::parse("F2").unwrap().key, Key::F(2));
        assert_eq!(Chord::parse("Ctrl++").unwrap(), Chord::ctrl(Key::Char('+')));
    }

    #[test]
    fn anything_else_is_refused_naming_the_bad_part() {
        assert!(
            Chord::parse("Ctl+P")
                .unwrap_err()
                .contains("'ctl' is not a modifier")
        );
        assert!(
            Chord::parse("F13")
                .unwrap_err()
                .contains("'F13' is not a key")
        );
        assert!(Chord::parse("PgUp").is_err());
        assert!(Chord::parse("").is_err());
    }
}

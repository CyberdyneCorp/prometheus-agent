//! Key bindings (spec: tui, Configurable key bindings): every action has a
//! default chord that `ui.tui.keys.<action>` may replace. Configuration
//! validation already refused unknown action names and malformed chords
//! (naming the key, exit 78); `/keys` lists the effective bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pmt_config::keys::{Chord, Key};
use pmt_config::schema::{KeyChord, TuiKeys};

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Send the composer.
    Submit,
    /// Insert a line break.
    Newline,
    /// Stop the running turn, else clear the composer.
    Interrupt,
    /// Leave when the composer is empty.
    Quit,
    /// Scroll the transcript a line up.
    ScrollUp,
    /// Scroll the transcript a line down.
    ScrollDown,
    /// Scroll the transcript a page up.
    PageUp,
    /// Scroll the transcript a page down.
    PageDown,
    /// The previous history entry.
    HistoryPrev,
    /// The next history entry.
    HistoryNext,
    /// Complete the word at the cursor.
    Complete,
    /// Move to the start of the line.
    LineStart,
    /// Move to the end of the line.
    LineEnd,
    /// Clear the composer.
    ClearInput,
}

/// One action: its `ui.tui.keys` name, its default chord, and its
/// configured chord in `TuiKeys`.
struct Spec {
    action: Action,
    name: &'static str,
    default: Chord,
    configured: fn(&TuiKeys) -> Option<&KeyChord>,
}

const SPECS: &[Spec] = &[
    Spec {
        action: Action::Submit,
        name: "submit",
        default: Chord::plain(Key::Enter),
        configured: |k| k.submit.as_ref(),
    },
    Spec {
        action: Action::Newline,
        name: "newline",
        default: Chord::alt(Key::Enter),
        configured: |k| k.newline.as_ref(),
    },
    Spec {
        action: Action::Interrupt,
        name: "interrupt",
        default: Chord::ctrl(Key::Char('c')),
        configured: |k| k.interrupt.as_ref(),
    },
    Spec {
        action: Action::Quit,
        name: "quit",
        default: Chord::ctrl(Key::Char('d')),
        configured: |k| k.quit.as_ref(),
    },
    Spec {
        action: Action::ScrollUp,
        name: "scroll_up",
        default: Chord::ctrl(Key::Up),
        configured: |k| k.scroll_up.as_ref(),
    },
    Spec {
        action: Action::ScrollDown,
        name: "scroll_down",
        default: Chord::ctrl(Key::Down),
        configured: |k| k.scroll_down.as_ref(),
    },
    Spec {
        action: Action::PageUp,
        name: "page_up",
        default: Chord::plain(Key::PageUp),
        configured: |k| k.page_up.as_ref(),
    },
    Spec {
        action: Action::PageDown,
        name: "page_down",
        default: Chord::plain(Key::PageDown),
        configured: |k| k.page_down.as_ref(),
    },
    Spec {
        action: Action::HistoryPrev,
        name: "history_prev",
        default: Chord::plain(Key::Up),
        configured: |k| k.history_prev.as_ref(),
    },
    Spec {
        action: Action::HistoryNext,
        name: "history_next",
        default: Chord::plain(Key::Down),
        configured: |k| k.history_next.as_ref(),
    },
    Spec {
        action: Action::Complete,
        name: "complete",
        default: Chord::plain(Key::Tab),
        configured: |k| k.complete.as_ref(),
    },
    Spec {
        action: Action::LineStart,
        name: "line_start",
        default: Chord::ctrl(Key::Char('a')),
        configured: |k| k.line_start.as_ref(),
    },
    Spec {
        action: Action::LineEnd,
        name: "line_end",
        default: Chord::ctrl(Key::Char('e')),
        configured: |k| k.line_end.as_ref(),
    },
    Spec {
        action: Action::ClearInput,
        name: "clear_input",
        default: Chord::ctrl(Key::Char('u')),
        configured: |k| k.clear_input.as_ref(),
    },
];

/// The effective bindings.
#[derive(Debug, Clone)]
pub struct Bindings {
    bound: Vec<(Action, &'static str, Chord)>,
}

impl Bindings {
    /// The defaults with `keys`' replacements.
    ///
    /// # Errors
    ///
    /// `ui.tui.keys.<action>: <reason>` for a chord that does not parse
    /// (validation refuses it earlier; this guards a bypassed check).
    pub fn new(keys: &TuiKeys) -> Result<Self, String> {
        let mut bound = Vec::with_capacity(SPECS.len());
        for spec in SPECS {
            let chord = match (spec.configured)(keys) {
                Some(text) => Chord::parse(&text.0)
                    .map_err(|why| format!("ui.tui.keys.{}: {why}", spec.name))?,
                None => spec.default,
            };
            bound.push((spec.action, spec.name, chord));
        }
        Ok(Self { bound })
    }

    /// The action `event` is bound to. `Shift+Enter` always inserts a line
    /// break, on terminals that report it.
    #[must_use]
    pub fn action(&self, event: &KeyEvent) -> Option<Action> {
        if event.kind == KeyEventKind::Release {
            return None;
        }
        let chord = chord_of(event)?;
        let bound = self
            .bound
            .iter()
            .find(|(_, _, c)| *c == chord)
            .map(|(action, _, _)| *action);
        bound.or_else(|| (chord == shift_enter()).then_some(Action::Newline))
    }

    /// `/keys`: each action's name and chord, one per line.
    #[must_use]
    pub fn describe(&self) -> String {
        self.bound
            .iter()
            .map(|(_, name, chord)| format!("{name:<14} {chord}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Default for Bindings {
    fn default() -> Self {
        Self::new(&TuiKeys::default()).expect("the defaults are valid")
    }
}

const fn shift_enter() -> Chord {
    Chord {
        shift: true,
        ..Chord::plain(Key::Enter)
    }
}

/// The chord a terminal key event stands for.
fn chord_of(event: &KeyEvent) -> Option<Chord> {
    let mods = event.modifiers;
    let key = match event.code {
        KeyCode::Char(c) => Key::Char(c.to_ascii_lowercase()),
        KeyCode::F(n) => Key::F(n),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        _ => return None,
    };
    // A typed capital is the character itself, not a Shift chord.
    let shifted_char = matches!(event.code, KeyCode::Char(_));
    Some(Chord {
        ctrl: mods.contains(KeyModifiers::CONTROL),
        alt: mods.contains(KeyModifiers::ALT),
        shift: (mods.contains(KeyModifiers::SHIFT) && !shifted_char)
            || event.code == KeyCode::BackTab,
        key,
    })
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pmt_config::schema::{KeyChord, TuiKeys};

    use super::{Action, Bindings};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn a_configured_chord_replaces_the_default() {
        let keys = TuiKeys {
            page_up: Some(KeyChord("Ctrl+B".into())),
            ..TuiKeys::default()
        };
        let bindings = Bindings::new(&keys).unwrap();
        assert_eq!(
            bindings.action(&key(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Some(Action::PageUp)
        );
        assert_eq!(
            bindings.action(&key(KeyCode::PageUp, KeyModifiers::NONE)),
            None
        );
        assert!(bindings.describe().contains("page_up        Ctrl+B"));
    }

    #[test]
    fn both_alt_and_shift_enter_insert_a_line_break() {
        let bindings = Bindings::default();
        assert_eq!(
            bindings.action(&key(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Action::Newline)
        );
        assert_eq!(
            bindings.action(&key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(Action::Newline)
        );
        assert_eq!(
            bindings.action(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Submit)
        );
        assert_eq!(
            bindings.action(&key(KeyCode::Char('P'), KeyModifiers::SHIFT)),
            None,
            "a capital letter is typed, not bound"
        );
    }
}

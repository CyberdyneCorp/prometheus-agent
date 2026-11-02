//! The OSC 11 query on a Windows console. Raw mode alone is not enough
//! there: the console turns a terminal's escape-sequence input into key
//! events of its own and drops what it cannot map, so the reply only
//! arrives, one key event per character, while virtual-terminal input
//! (`ENABLE_VIRTUAL_TERMINAL_INPUT`) is on. The mode is set for the query
//! and put back afterwards, so the key reader sees the console as it was.

use std::time::{Duration, Instant};

use crossterm_winapi::{Console, ConsoleMode, Handle, InputRecord};
use pmt_config::schema::TuiTheme;

use super::{QUERY, QUERY_TIMEOUT, complete, from_osc11};

/// `ENABLE_VIRTUAL_TERMINAL_INPUT` (wincon.h).
const VT_INPUT: u32 = 0x0200;

/// How often the input buffer is checked while waiting. The console API
/// has no read with a timeout; polling keeps the wait bounded without one.
const POLL: Duration = Duration::from_millis(5);

/// Asks the console for its background colour (see the Unix
/// [`query`](super) for the contract): `None` without a console, or when
/// no reply comes within [`QUERY_TIMEOUT`].
#[must_use]
pub fn query() -> Option<TuiTheme> {
    let input = Handle::current_in_handle().ok()?;
    let mode = ConsoleMode::from(input.clone());
    let saved = mode.mode().ok()?;
    mode.set_mode(saved | VT_INPUT).ok()?;
    let theme = ask(&mut std::io::stdout(), &Console::from(input));
    // Best effort: the console stays usable either way.
    let _ = mode.set_mode(saved);
    theme
}

/// Writes the query to `out` and reads the reply from `console`. Only
/// the reply's events are read: input after it stays for the key reader.
fn ask(out: &mut impl std::io::Write, console: &Console) -> Option<TuiTheme> {
    out.write_all(QUERY).ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut reply = Vec::new();
    while Instant::now() < deadline {
        if console.number_of_console_input_events().ok()? == 0 {
            std::thread::sleep(POLL);
            continue;
        }
        if let InputRecord::KeyEvent(key) = console.read_single_input_event().ok()? {
            push_char(&mut reply, key.key_down, key.u_char);
        }
        if complete(&reply) {
            return from_osc11(&reply);
        }
    }
    None
}

/// Appends a key-down event's character (UTF-16 code unit) to `reply`.
/// The reply is ASCII; anything else cannot be part of it and is kept
/// only so that it does not pass for an ASCII byte.
fn push_char(reply: &mut Vec<u8>, key_down: bool, unit: u16) {
    if !key_down || unit == 0 {
        return;
    }
    match u8::try_from(unit) {
        Ok(byte) if byte.is_ascii() => reply.push(byte),
        _ => reply.push(0xff),
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::super::console_input::{self, Restore};
    use super::{VT_INPUT, query};

    /// One test: the console input buffer is shared by the whole process.
    #[test]
    fn a_console_reply_is_read_within_the_timeout_and_the_mode_restored() {
        let console = console_input::open();
        let restore = Restore::new(&console, VT_INPUT);
        let before = console.mode();

        // A light background, then a key typed after it.
        console.flush();
        console.type_text("\u{1b}]11;rgb:ffff/ffff/ffff\u{1b}\\x");
        assert_eq!(query(), Some(TuiTheme::Light));
        assert_eq!(console.mode(), before, "the mode is put back");
        assert_eq!(
            console.pending(),
            1,
            "the later key stays for the key reader"
        );

        console.flush();
        console.type_text("\u{1b}]11;rgb:1c1c/1c1c/1c1c\u{7}");
        assert_eq!(query(), Some(TuiTheme::Dark));

        // No reply: bounded by the timeout.
        console.flush();
        let started = std::time::Instant::now();
        assert_eq!(query(), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(console.mode(), before);
        drop(restore);
    }
}

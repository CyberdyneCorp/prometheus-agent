//! The TUI in a pseudo-terminal: spawning it, reading what it drew.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

/// Entering and leaving the alternate screen.
pub const ENTER: &str = "\u{1b}[?1049h";
pub const LEAVE: &str = "\u{1b}[?1049l";

/// The binary on a terminal with `ui.interface` at its default (`tui`).
pub fn tui(home: &Path, args: &[&str], env: &[(&str, &str)]) -> OsSession {
    tui_with(home, args, env, &[])
}

/// [`tui`] with the variables `removed` unset too.
pub fn tui_with(home: &Path, args: &[&str], env: &[(&str, &str)], removed: &[&str]) -> OsSession {
    let mut cmd: Command = super::command(home);
    cmd.args(args)
        .current_dir(home)
        .env("TERM", "xterm-256color")
        // The developer's terminal must not pick the theme.
        .env_remove("COLORFGBG")
        .env_remove("NO_COLOR");
    for key in removed {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut session = OsSession::spawn(cmd).unwrap();
    session.get_process_mut().set_window_size(100, 30).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// Everything the process writes until it exits, and its exit status.
pub fn finish(mut p: OsSession) -> (String, Option<i32>) {
    let found = p.expect(Eof).unwrap();
    // Eof matches everything left in the buffer.
    let text = String::from_utf8_lossy(found.as_bytes()).into_owned();
    let status = format!("{:?}", p.get_process_mut().wait().unwrap());
    // `Exited(Pid(n), code)`; a signal death has no exit code.
    let code = status
        .strip_prefix("Exited(")
        .and_then(|rest| rest.trim_end_matches(')').rsplit(", ").next())
        .and_then(|code| code.parse().ok());
    (text, code)
}

/// Waits until the screen shows `text`: its words in order, with only
/// cursor moves or blanks between them (the TUI redraws only the cells
/// that change, so the spaces of a line are not always written). Returns
/// everything read up to and including it.
pub fn see(p: &mut OsSession, text: &str) -> String {
    try_see(p, text).unwrap()
}

/// [`see`] that reports a timeout instead of panicking.
fn try_see(p: &mut OsSession, text: &str) -> Result<String, expectrl::Error> {
    let escape = |word: &str| {
        word.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_string()
                } else {
                    format!("\\x{{{:x}}}", u32::from(c))
                }
            })
            .collect::<String>()
    };
    let gap = r"(?:\x1b\[[0-9;?]*[A-Za-z]| )*";
    let pattern = text
        .split_whitespace()
        .map(escape)
        .collect::<Vec<_>>()
        .join(gap);
    let found = p.expect(expectrl::Regex(pattern))?;
    let mut read = String::from_utf8_lossy(found.before()).into_owned();
    for matched in found.matches() {
        read.push_str(&String::from_utf8_lossy(matched));
    }
    Ok(read)
}

/// Waits until a full repaint shows `text`, typing `/redraw` until it
/// does (30 s at most). For screen text that arrives asynchronously, such
/// as the status line's model (`model.options`) or battery: drawn into an
/// existing frame, only its changed cells are written, so the whole line
/// is on the wire only after a repaint that follows its arrival.
pub fn see_repainted(p: &mut OsSession, text: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    // A timeout leaves the unread output buffered, so each try rescans it.
    p.set_expect_timeout(Some(Duration::from_secs(1)));
    let found = loop {
        enter(p, "/redraw");
        match try_see(p, text) {
            Ok(found) => break found,
            Err(expectrl::Error::ExpectTimeout) if std::time::Instant::now() < deadline => {}
            Err(err) => panic!("{text:?} never shown after a repaint: {err}"),
        }
    };
    p.set_expect_timeout(Some(Duration::from_secs(30)));
    found
}

/// Types `text` and Enter (a carriage return, as a terminal sends it).
pub fn enter(p: &mut OsSession, text: &str) {
    p.send(format!("{text}\r")).unwrap();
}

/// Waits for the TUI's composer on the alternate screen.
pub fn opened(p: &mut OsSession) {
    p.expect(ENTER).unwrap();
    p.expect("default").unwrap();
}

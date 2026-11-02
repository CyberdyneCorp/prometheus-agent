//! The terminal while the TUI runs (spec: tui, Terminal state is always
//! restored): raw mode, the alternate screen, mouse capture and bracketed
//! paste are switched on by [`TerminalGuard::enter`] and off again by
//! [`restore`], which every exit path runs: dropping the guard (normal
//! exit and errors), the binary's signal handling, and the panic hook of
//! [`install_panic_hook`], which also writes the panic report to
//! `<home>/logs/tui-panic-<timestamp>.log` and names it on stderr.

use std::backtrace::Backtrace;
use std::io::{self, Write as _};
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Whether the terminal is in the TUI's modes now.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The terminal in the TUI's modes until dropped.
#[derive(Debug)]
pub struct TerminalGuard(());

impl TerminalGuard {
    /// Switches the terminal into the TUI's modes.
    ///
    /// # Errors
    ///
    /// The terminal refused a mode (whatever was switched on is restored).
    pub fn enter() -> io::Result<Self> {
        ACTIVE.store(true, Ordering::SeqCst);
        let entered = enable_raw_mode().and_then(|()| {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture
            )
        });
        match entered {
            Ok(()) => Ok(Self(())),
            Err(err) => {
                restore();
                Err(err)
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Leaves raw mode and the alternate screen, shows the cursor and turns
/// mouse capture and bracketed paste off; `true` when the TUI's modes were
/// on. Running it again does nothing.
pub fn restore() -> bool {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return false;
    }
    let mut out = io::stdout();
    let _ = execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
    let _ = disable_raw_mode();
    let _ = out.flush();
    true
}

/// Whether the TUI's modes are on.
#[must_use]
pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Installs a panic hook that, when the TUI has the terminal, restores it
/// first, writes the report to `<logs_dir>/tui-panic-<timestamp>.log` and
/// prints its path on stderr, then hands the panic to the hook installed
/// before (which reports it as the process's crash).
pub fn install_panic_hook(logs_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        if restore() {
            match write_report(&logs_dir, info) {
                Ok(path) => eprintln!(
                    "prometheus tui crashed; the panic report is in {}",
                    path.display()
                ),
                Err(err) => eprintln!("prometheus tui crashed (no panic report: {err})"),
            }
        }
        previous(info);
    }));
}

fn write_report(dir: &Path, info: &PanicHookInfo<'_>) -> io::Result<PathBuf> {
    pmt_core::private_fs::create_dir(dir)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.3fZ");
    let path = dir.join(format!("tui-panic-{stamp}.log"));
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_owned());
    let location = info
        .location()
        .map_or_else(|| "unknown".to_owned(), ToString::to_string);
    let mut file = pmt_core::private_fs::new_file_options().open(&path)?;
    writeln!(
        file,
        "prometheus {} TUI panic\nmessage: {message}\nlocation: {location}\n\n{}",
        env!("CARGO_PKG_VERSION"),
        Backtrace::force_capture()
    )?;
    Ok(path)
}

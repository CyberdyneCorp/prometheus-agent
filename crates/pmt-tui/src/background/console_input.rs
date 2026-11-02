//! Test support (Windows): the process's own console, with input typed
//! into it the way a terminal delivers a reply. Writing console input
//! needs `WriteConsoleInputW`, which no safe wrapper offers; this module
//! is allowlisted for `unsafe` in health.toml and compiled for tests only.

#![allow(unsafe_code)]

use crossterm_winapi::{Console, ConsoleMode, Handle};
use windows_sys::Win32::System::Console::{
    AllocConsole, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0,
    WriteConsoleInputW,
};

/// The console input buffer (`CONIN$`).
pub(super) struct ConsoleInput {
    handle: Handle,
}

/// The console of the test process, created when it has none (a CI
/// runner may start tests without one).
pub(super) fn open() -> ConsoleInput {
    let handle = Handle::current_in_handle().unwrap_or_else(|_| {
        // SAFETY: no arguments; it fails harmlessly when a console exists.
        unsafe { AllocConsole() };
        Handle::current_in_handle().expect("a console after AllocConsole")
    });
    ConsoleInput { handle }
}

impl ConsoleInput {
    pub(super) fn mode(&self) -> u32 {
        ConsoleMode::from(self.handle.clone()).mode().unwrap()
    }

    pub(super) fn set_mode(&self, mode: u32) {
        ConsoleMode::from(self.handle.clone())
            .set_mode(mode)
            .unwrap();
    }

    /// Events waiting to be read.
    pub(super) fn pending(&self) -> u32 {
        Console::from(self.handle.clone())
            .number_of_console_input_events()
            .unwrap()
    }

    /// Drops every waiting event.
    pub(super) fn flush(&self) {
        let console = Console::from(self.handle.clone());
        while self.pending() > 0 {
            console.read_console_input().unwrap();
        }
    }

    /// Queues one key-down event per UTF-16 unit of `text`.
    pub(super) fn type_text(&self, text: &str) {
        let records: Vec<INPUT_RECORD> = text.encode_utf16().map(key_down).collect();
        let mut written = 0;
        // SAFETY: `records` is a live slice of initialised INPUT_RECORDs
        // whose length is passed with it; `written` outlives the call; the
        // handle is an open CONIN$ handle with write access.
        let ok = unsafe {
            WriteConsoleInputW(
                (*self.handle).cast(),
                records.as_ptr(),
                u32::try_from(records.len()).unwrap(),
                &raw mut written,
            )
        };
        assert!(
            ok != 0 && written as usize == records.len(),
            "console input written"
        );
    }
}

/// Puts the console's input mode back when the test ends, and starts it
/// without virtual-terminal input.
pub(super) struct Restore<'a> {
    console: &'a ConsoleInput,
    mode: u32,
}

impl<'a> Restore<'a> {
    pub(super) fn new(console: &'a ConsoleInput, without: u32) -> Self {
        let mode = console.mode();
        console.set_mode(mode & !without);
        Self { console, mode }
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.console.flush();
        self.console.set_mode(self.mode);
    }
}

fn key_down(unit: u16) -> INPUT_RECORD {
    INPUT_RECORD {
        EventType: u16::try_from(KEY_EVENT).unwrap(),
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: 1,
                wRepeatCount: 1,
                wVirtualKeyCode: 0,
                wVirtualScanCode: 0,
                uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                dwControlKeyState: 0,
            },
        },
    }
}

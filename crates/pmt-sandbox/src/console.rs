//! A UTF-8 console for the CLI and its children on Windows (spec:
//! execution-environments, Windows local shell): the console's input and
//! output code pages become 65001 for the life of the guard and the
//! original ones come back when it drops. Children share the console, so
//! they read and write UTF-8 too. Elsewhere this does nothing.
//!
//! On Windows it also interrupts one child the way a user's Ctrl+Break
//! would ([`new_process_group`], [`ctrl_break`]): a console control event
//! reaches a whole process group, and Ctrl+C cannot target a single group
//! (a new group starts with Ctrl+C ignored), so the child gets a group of
//! its own and Ctrl+Break.

/// The UTF-8 code page.
pub const UTF8_CODE_PAGE: u32 = 65_001;

/// Restores the original console code pages when dropped.
#[derive(Debug)]
#[must_use = "the code pages are restored when the guard drops"]
pub struct Utf8Console {
    /// `(input, output)` code pages to restore; `None` without a console.
    saved: Option<(u32, u32)>,
}

impl Utf8Console {
    /// Switches an attached console to UTF-8.
    pub fn enable() -> Self {
        let saved = code_pages();
        if saved.is_some_and(|pages| pages != (UTF8_CODE_PAGE, UTF8_CODE_PAGE)) {
            set_code_pages((UTF8_CODE_PAGE, UTF8_CODE_PAGE));
        }
        Self { saved }
    }
}

impl Drop for Utf8Console {
    fn drop(&mut self) {
        if let Some(pages) = self.saved {
            set_code_pages(pages);
        }
    }
}

/// The console's `(input, output)` code pages, `None` without a console.
#[must_use]
pub fn code_pages() -> Option<(u32, u32)> {
    #[cfg(windows)]
    return crate::platform::console::code_pages();
    #[cfg(not(windows))]
    None
}

fn set_code_pages(pages: (u32, u32)) {
    #[cfg(windows)]
    crate::platform::console::set_code_pages(pages);
    #[cfg(not(windows))]
    let _ = pages;
}

/// Starts the child of `cmd` in a console process group of its own, so
/// [`ctrl_break`] can reach it without reaching this process.
#[cfg(windows)]
pub fn new_process_group(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
}

/// Sends Ctrl+Break to the process group of a child started with
/// [`new_process_group`] (its id is the child's process id). It shares
/// this process's console.
///
/// # Errors
///
/// The OS error when this process has no console or the group is gone.
#[cfg(windows)]
pub fn ctrl_break(group: u32) -> std::io::Result<()> {
    crate::platform::console::ctrl_break(group)
}

#[cfg(test)]
mod tests {
    use super::{UTF8_CODE_PAGE, Utf8Console, code_pages};

    #[test]
    fn code_pages_are_utf8_while_enabled_and_restored_after() {
        let before = code_pages();
        {
            let _console = Utf8Console::enable();
            if before.is_some() {
                assert_eq!(code_pages(), Some((UTF8_CODE_PAGE, UTF8_CODE_PAGE)));
            }
        }
        assert_eq!(code_pages(), before);
    }
}

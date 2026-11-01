//! A UTF-8 console for the CLI and its children on Windows (spec:
//! execution-environments, Windows local shell): the console's input and
//! output code pages become 65001 for the life of the guard and the
//! original ones come back when it drops. Children share the console, so
//! they read and write UTF-8 too. Elsewhere this does nothing.

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

//! The line interface's bottom status line (spec: tui, Client-local
//! display commands: "in `line` mode a single bottom status line on a
//! TTY"). The last terminal row is kept out of the scrolling region and
//! the status is written there; the editor thread draws it right before
//! each read, when nothing else is writing to the terminal, and the region
//! is given back when the line is turned off or the interface ends.

use std::fmt::Write as _;
use std::io::{IsTerminal as _, Write as _};

use unicode_width::UnicodeWidthChar as _;

/// What is on the bottom row.
#[derive(Debug, Default)]
pub(super) struct BottomLine {
    drawn: bool,
}

impl BottomLine {
    /// Draws `text` on the last row, or gives the row back for `None`.
    pub(super) fn show(&mut self, text: Option<&str>) {
        let out = std::io::stdout();
        if !out.is_terminal() {
            return;
        }
        let Ok((columns, rows)) = crossterm::terminal::size() else {
            return;
        };
        let mut sequence = String::new();
        match text {
            Some(text) if rows >= 3 => {
                if !self.drawn {
                    // Room for the row, which the cursor may be on: index
                    // down (scrolling at the bottom), then reverse index.
                    sequence.push_str("\x1bD\x1bM");
                }
                let shown = fit(text, usize::from(columns).saturating_sub(1));
                // DECSTBM homes the cursor: save and restore around it.
                let _ = write!(
                    sequence,
                    "\x1b7\x1b[1;{}r\x1b8\x1b7\x1b[{rows};1H\x1b[2K{shown}\x1b8",
                    rows - 1
                );
                self.drawn = true;
            }
            _ if self.drawn => {
                sequence.push_str(&release(rows));
                self.drawn = false;
            }
            _ => return,
        }
        let mut out = out.lock();
        let _ = out.write_all(sequence.as_bytes());
        let _ = out.flush();
    }

    /// Gives the row back (the interface ends).
    pub(super) fn release(&mut self) {
        self.show(None);
    }
}

/// Resets the scrolling region and blanks the last row.
fn release(rows: u16) -> String {
    format!("\x1b7\x1b[r\x1b[{rows};1H\x1b[2K\x1b8")
}

/// `text` cut to `width` display columns.
fn fit(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{fit, release};

    #[test]
    fn the_line_is_cut_to_the_terminal_and_released_whole() {
        assert_eq!(fit("default │ mock/mock", 9), "default │");
        assert_eq!(release(24), "\x1b7\x1b[r\x1b[24;1H\x1b[2K\x1b8");
    }
}

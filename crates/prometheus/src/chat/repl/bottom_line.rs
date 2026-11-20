//! The line interface's bottom area (spec: tui, Client-local display
//! commands: "in `line` mode a single bottom status line on a TTY"; pets,
//! Terminal pet rendering): the status line on the last row and, above
//! it, the pet pane. Those rows are kept out of the scrolling region and
//! redrawn there: by the editor thread right before each read, when
//! nothing else is writing to the terminal, and by the status and pet
//! handles between reads. The region is given back when both are off or
//! the interface ends.

use std::fmt::Write as _;
use std::io::{IsTerminal as _, Write as _};

use pmt_pets::render::{self, Sprite};
use unicode_width::UnicodeWidthChar as _;

/// The pet pane's frame and whether the heart shows beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PetFrame {
    /// The frame.
    pub sprite: Sprite,
    /// The affection reaction plays.
    pub heart: bool,
}

/// What the bottom rows hold.
#[derive(Debug, Default)]
pub(super) struct BottomLine {
    /// Rows kept out of the scrolling region.
    reserved: u16,
}

impl BottomLine {
    /// Draws `text` on the last row and `pet` above it; gives the rows
    /// back when both are `None`.
    pub(super) fn show(&mut self, text: Option<&str>, pet: Option<&PetFrame>) {
        let out = std::io::stdout();
        if !out.is_terminal() {
            return;
        }
        let Ok((columns, rows)) = crossterm::terminal::size() else {
            return;
        };
        let sequence = self.sequence(text, pet, columns, rows);
        if sequence.is_empty() {
            return;
        }
        let mut out = out.lock();
        let _ = out.write_all(sequence.as_bytes());
        let _ = out.flush();
    }

    /// What draws the area on a `columns` x `rows` terminal.
    fn sequence(
        &mut self,
        text: Option<&str>,
        pet: Option<&PetFrame>,
        columns: u16,
        rows: u16,
    ) -> String {
        let status = u16::from(text.is_some());
        // The pet needs its rows and columns; the conversation keeps at
        // least three rows.
        let pet =
            pet.filter(|p| p.sprite.rows + status + 3 <= rows && p.sprite.cols + 3 <= columns);
        let wanted = status + pet.map_or(0, |p| p.sprite.rows);
        if wanted == 0 || rows < 3 {
            if self.reserved == 0 {
                return String::new();
            }
            let sequence = release(rows, self.reserved);
            self.reserved = 0;
            return sequence;
        }
        let mut sequence = String::new();
        if wanted > self.reserved {
            // Room for the new rows, which the cursor may be on: index
            // down (scrolling at the bottom), then reverse index.
            let grow = usize::from(wanted - self.reserved);
            sequence.push_str(&"\x1bD".repeat(grow));
            sequence.push_str(&"\x1bM".repeat(grow));
        }
        let first = rows - wanted.max(self.reserved) + 1;
        // DECSTBM homes the cursor: save and restore around it.
        let _ = write!(sequence, "\x1b7\x1b[1;{}r\x1b8\x1b7\x1b[0m", rows - wanted);
        for row in first..=rows {
            let _ = write!(sequence, "\x1b[{row};1H\x1b[2K");
        }
        sequence.push_str("\x1b8");
        if let Some(pet) = pet {
            let top = rows - wanted + 1;
            let col = columns - pet.sprite.cols;
            if pet.heart {
                let at = col.saturating_sub(2).max(1);
                let _ = write!(sequence, "\x1b7\x1b[{top};{at}H\x1b[31m♥\x1b[0m\x1b8");
            }
            sequence.push_str(&render::draw_at(&pet.sprite, top, col));
        }
        if let Some(text) = text {
            let shown = fit(text, usize::from(columns).saturating_sub(1));
            let _ = write!(sequence, "\x1b7\x1b[{rows};1H{shown}\x1b8");
        }
        self.reserved = wanted;
        sequence
    }

    /// Gives the rows back (the interface ends).
    pub(super) fn release(&mut self) {
        self.show(None, None);
    }
}

/// Resets the scrolling region and blanks the `reserved` last rows.
fn release(rows: u16, reserved: u16) -> String {
    let mut sequence = String::from("\x1b7\x1b[r");
    for row in rows.saturating_sub(reserved) + 1..=rows {
        let _ = write!(sequence, "\x1b[{row};1H\x1b[2K");
    }
    sequence.push_str("\x1b8");
    sequence
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
    use pmt_pets::render::{Body, Sprite};

    use super::{BottomLine, PetFrame, fit, release};

    #[test]
    fn the_line_is_cut_to_the_terminal_and_released_whole() {
        assert_eq!(fit("default │ mock/mock", 9), "default │");
        assert_eq!(release(24, 1), "\x1b7\x1b[r\x1b[24;1H\x1b[2K\x1b8");
    }

    fn pet(rows: u16) -> PetFrame {
        PetFrame {
            sprite: Sprite {
                cols: 4,
                rows,
                body: Body::Lines((0..rows).map(|r| format!("p{r}")).collect()),
            },
            heart: false,
        }
    }

    #[test]
    fn the_pet_pane_sits_above_the_status_line_and_is_released() {
        let mut area = BottomLine::default();
        let drawn = area.sequence(Some("status"), Some(&pet(2)), 80, 24);
        assert!(drawn.contains("\x1b[1;21r"), "{drawn:?}");
        assert!(
            drawn.contains("\x1b[22;76Hp0") && drawn.contains("\x1b[23;76Hp1"),
            "{drawn:?}"
        );
        assert!(drawn.contains("\x1b[24;1Hstatus"));
        let released = area.sequence(None, None, 80, 24);
        assert!(released.starts_with("\x1b7\x1b[r") && released.contains("\x1b[22;1H\x1b[2K"));
        assert!(area.sequence(None, None, 80, 24).is_empty());
    }

    #[test]
    fn a_pet_taller_than_the_terminal_is_left_out() {
        let mut area = BottomLine::default();
        assert!(area.sequence(None, Some(&pet(30)), 80, 24).is_empty());
    }
}

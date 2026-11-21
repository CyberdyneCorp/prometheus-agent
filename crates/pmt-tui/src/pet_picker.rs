//! The pet picker (spec: pets, Pet slash command): `/pet list` lists the
//! installed pets and the gallery's; `Enter` adopts an installed one, or
//! installs a gallery pet and adopts it.

use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::truncate;

/// One pet to pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PetChoice {
    /// Its slug.
    pub slug: String,
    /// Its name.
    pub name: String,
    /// Installed already (else a gallery pet).
    pub installed: bool,
}

/// The open picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PetPicker {
    /// The installed pets, then the gallery's not installed.
    pub choices: Vec<PetChoice>,
    /// The highlighted row.
    pub selected: usize,
    /// The pet shown now.
    pub current: Option<String>,
    /// The gallery is still being read (or why it could not be).
    pub gallery: Option<String>,
}

impl PetPicker {
    /// The picker over the installed pets, the gallery still loading.
    #[must_use]
    pub fn new(installed: Vec<PetChoice>, current: Option<String>, loading: &str) -> Self {
        let selected = installed
            .iter()
            .position(|c| Some(&c.slug) == current.as_ref())
            .unwrap_or(0);
        Self {
            choices: installed,
            selected,
            current,
            gallery: Some(loading.to_owned()),
        }
    }

    /// Adds the gallery's pets that are not installed.
    pub fn add_gallery(&mut self, gallery: Vec<PetChoice>) {
        let more: Vec<PetChoice> = gallery
            .into_iter()
            .filter(|g| !self.choices.iter().any(|c| c.slug == g.slug))
            .collect();
        self.choices.extend(more);
        self.gallery = None;
    }

    /// Moves the highlight `by` rows, staying on the list.
    pub fn move_selection(&mut self, by: isize) {
        let last = self.choices.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    /// The highlighted pet.
    #[must_use]
    pub fn chosen(&self) -> Option<&PetChoice> {
        self.choices.get(self.selected)
    }

    /// The overlay: a hint, the gallery's state, then one row per pet
    /// (`›` the shown one, `↓` a gallery pet to install).
    #[must_use]
    pub fn lines(
        &self,
        width: u16,
        rows: usize,
        theme: &Theme,
        strings: &pmt_i18n::Strings,
    ) -> Vec<Line<'static>> {
        let width = usize::from(width).max(1);
        let mut lines = vec![Line::from(Span::styled(
            truncate(strings.get("tui.pet_hint"), width),
            theme.card,
        ))];
        if let Some(state) = &self.gallery {
            lines.push(Line::from(Span::styled(
                truncate(state, width),
                theme.frame,
            )));
        }
        let shown = rows.saturating_sub(lines.len()).max(1);
        let first = (self.selected + 1).saturating_sub(shown);
        for (n, choice) in self.choices.iter().enumerate().skip(first).take(shown) {
            let mark = if Some(&choice.slug) == self.current.as_ref() {
                '›'
            } else if choice.installed {
                ' '
            } else {
                '↓'
            };
            let text = format!("{mark} {}  {}", choice.slug, choice.name);
            let style = if n == self.selected {
                theme.selected
            } else {
                theme.card
            };
            lines.push(Line::from(Span::styled(truncate(&text, width), style)));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(slug: &str, installed: bool) -> PetChoice {
        PetChoice {
            slug: slug.into(),
            name: slug.to_uppercase(),
            installed,
        }
    }

    #[test]
    fn gallery_pets_join_after_the_installed_ones_without_duplicates() {
        let mut picker = PetPicker::new(
            vec![choice("boba", true), choice("mochi", true)],
            Some("mochi".into()),
            "loading the gallery…",
        );
        assert_eq!(picker.chosen().unwrap().slug, "mochi");
        picker.add_gallery(vec![choice("boba", false), choice("taro", false)]);
        assert_eq!(picker.choices.len(), 3);
        picker.move_selection(5);
        assert_eq!(picker.chosen().unwrap().slug, "taro");
        let strings = pmt_i18n::Strings::english(pmt_i18n::Surface::Tui);
        let theme = Theme::new(pmt_config::schema::TuiTheme::Dark, true);
        let text: Vec<String> = picker
            .lines(40, 10, &theme, &strings)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(text.iter().any(|l| l.starts_with("› mochi")), "{text:?}");
        assert!(text.iter().any(|l| l.starts_with("↓ taro")), "{text:?}");
    }
}

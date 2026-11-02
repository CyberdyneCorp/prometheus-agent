//! The startup banner (spec: tui, Collapsible startup banner): what the
//! session runs with, grouped into sections (Tools open, Skills, System
//! Prompt and MCP Servers collapsed) whose headers a click or `Enter`
//! toggles. The toggles live in this banner only: the next launch starts
//! from the defaults again.

use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::transcript::Kind;
use crate::wrap::{truncate, wrap};

/// One section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Its header.
    pub title: String,
    /// What it lists.
    pub items: Vec<String>,
    /// How its items are shown when open: joined on one wrapped line
    /// (names) or one per line (text).
    pub joined: bool,
    /// Shown open.
    pub open: bool,
}

/// The banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    /// The first line (version, profile, model).
    pub title: String,
    /// The sections, in order.
    pub sections: Vec<Section>,
    /// The header `Enter` toggles (moved to with the banner key and the
    /// arrows), if any.
    pub focus: Option<usize>,
}

/// What the banner lists.
#[derive(Debug, Clone, Default)]
pub struct Contents {
    /// The session's tools.
    pub tools: Vec<String>,
    /// Installed skills.
    pub skills: Vec<String>,
    /// The frozen system prompt's lines (empty before the first turn).
    pub system_prompt: Vec<String>,
    /// MCP servers and their state.
    pub mcp_servers: Vec<String>,
}

impl Banner {
    /// The banner with the default toggles: Tools open, the rest closed.
    #[must_use]
    pub fn new(title: String, contents: Contents) -> Self {
        let section = |title: &str, items: Vec<String>, joined: bool, open: bool| Section {
            title: title.to_owned(),
            items,
            joined,
            open,
        };
        Self {
            title,
            sections: vec![
                section("Tools", contents.tools, true, true),
                section("Skills", contents.skills, true, false),
                section("System Prompt", contents.system_prompt, false, false),
                section("MCP Servers", contents.mcp_servers, true, false),
            ],
            focus: None,
        }
    }

    /// Opens or closes section `index`.
    pub fn toggle(&mut self, index: usize) {
        if let Some(section) = self.sections.get_mut(index) {
            section.open = !section.open;
        }
    }

    /// Moves the focus `by` headers (wrapping), starting at the first.
    pub fn move_focus(&mut self, by: isize) {
        let n = self.sections.len().cast_signed();
        let at = self
            .focus
            .map_or(0, |f| (f.cast_signed() + by).rem_euclid(n));
        self.focus = Some(at.cast_unsigned());
    }

    /// The banner's lines for `width` columns; header lines carry their
    /// section's index (a click on one toggles it).
    #[must_use]
    pub fn lines(&self, width: u16, theme: &Theme) -> Vec<(Line<'static>, Option<usize>)> {
        let width = usize::from(width).max(1);
        let mut lines = vec![(
            Line::from(Span::styled(
                truncate(&self.title, width),
                theme.entry(Kind::Notice),
            )),
            None,
        )];
        for (index, section) in self.sections.iter().enumerate() {
            let marker = if section.open { '▾' } else { '▸' };
            let header = format!("{marker} {} ({})", section.title, section.items.len());
            let style = if self.focus == Some(index) {
                theme.selected
            } else {
                theme.frame
            };
            lines.push((
                Line::from(Span::styled(truncate(&header, width), style)),
                Some(index),
            ));
            if section.open {
                for text in section.body(width.saturating_sub(2)) {
                    lines.push((Line::from(format!("  {text}")), None));
                }
            }
        }
        lines
    }
}

impl Section {
    fn body(&self, width: usize) -> Vec<String> {
        if self.items.is_empty() {
            return vec!["(none)".to_owned()];
        }
        if self.joined {
            return wrap(&self.items.join(", "), width);
        }
        self.items
            .iter()
            .flat_map(|item| wrap(item, width))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::{Banner, Contents};
    use crate::theme::Theme;

    fn shown(banner: &Banner) -> Vec<String> {
        banner
            .lines(60, &Theme::new(TuiTheme::Dark, true))
            .iter()
            .map(|(line, _)| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// Scenario: Many skills installed.
    #[test]
    fn eighty_skills_show_only_their_header_and_count_until_opened() {
        let contents = Contents {
            tools: vec!["read_file".into(), "terminal".into()],
            skills: (0..80).map(|n| format!("skill-{n}")).collect(),
            ..Contents::default()
        };
        let mut banner = Banner::new("Prometheus".into(), contents);
        let lines = shown(&banner);
        assert_eq!(
            lines,
            [
                "Prometheus",
                "▾ Tools (2)",
                "  read_file, terminal",
                "▸ Skills (80)",
                "▸ System Prompt (0)",
                "▸ MCP Servers (0)"
            ]
        );
        banner.toggle(1);
        assert!(shown(&banner).iter().any(|l| l.contains("skill-79")));
        banner.move_focus(1);
        banner.move_focus(-1);
        assert_eq!(banner.focus, Some(3), "wraps around");
    }
}

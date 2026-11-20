//! The status line (spec: tui, Status line and accessibility; Client-local
//! display commands; Busy indicator styles; Live session switcher): the
//! battery when it is shown, profile, model, context-window use as a
//! percentage, the session's cost in the ledger currency, its billing
//! source and subscription quota, the pending
//! approvals and the live sessions, the local runtime's GPU and RAM use
//! while a local model is resident (spec: local-models, Local resource
//! telemetry), led by the busy indicator while a turn runs. Items that do
//! not fit a narrow terminal are dropped from the end.

use std::time::Duration;

use pmt_config::schema::BusyIndicator;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use pmt_i18n::{Strings, t};

use crate::busy;
use crate::theme::Theme;
use crate::wrap::truncate;
use pmt_core::battery::Battery;

/// Between two items.
const SEPARATOR: &str = " │ ";

/// Bytes in a GiB (the unit of the resource items).
const GIB: f64 = 1_073_741_824.0;

/// The local runtime's resource use, from `session.context_usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LocalResources {
    /// How busy the GPU is, percent, when the driver reports it.
    pub gpu_percent: Option<f64>,
    /// GPU memory in use, bytes.
    pub gpu_used: u64,
    /// GPU memory, bytes (0: no GPU).
    pub gpu_total: u64,
    /// System RAM in use, bytes.
    pub ram_used: u64,
    /// System RAM, bytes.
    pub ram_total: u64,
}

impl LocalResources {
    /// The GPU item's value (`37% 2.0/24.0 GiB`), when there is a GPU.
    fn gpu(&self) -> Option<String> {
        if self.gpu_total == 0 {
            return None;
        }
        let memory = gib_pair(self.gpu_used, self.gpu_total);
        Some(match self.gpu_percent {
            Some(percent) => format!("{percent:.0}% {memory}"),
            None => memory,
        })
    }

    /// The RAM item's value (`16.0/32.0 GiB`).
    fn ram(&self) -> String {
        gib_pair(self.ram_used, self.ram_total)
    }
}

/// `used/total GiB` with one decimal.
#[allow(clippy::cast_precision_loss)] // display only
fn gib_pair(used: u64, total: u64) -> String {
    format!("{:.1}/{:.1} GiB", used as f64 / GIB, total as f64 / GIB)
}

/// What the status line shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// The battery, when shown and the machine has one.
    pub battery: Option<Battery>,
    /// The profile.
    pub profile: String,
    /// The session's model (`provider/model`).
    pub model: Option<String>,
    /// Context tokens in use.
    pub context_tokens: Option<u64>,
    /// The model's context window.
    pub context_window: Option<u64>,
    /// The session's cost in USD.
    pub cost_usd: Option<f64>,
    /// What the current route bills (`subscription:openai-codex/personal`).
    pub billing: Option<String>,
    /// The billed subscription's quota line.
    pub quota: Option<String>,
    /// Approvals waiting for an answer.
    pub approvals: usize,
    /// Sessions this TUI is attached to (shown from two on).
    pub live: usize,
    /// Who else is attached to the shown session, and whether this TUI
    /// only observes it (`with desktop-7`, `observing`).
    pub shared: Option<String>,
    /// A turn runs.
    pub busy: bool,
    /// How long the busy period has lasted.
    pub busy_for: Duration,
    /// The busy indicator's style.
    pub indicator: BusyIndicator,
    /// No animation.
    pub reduced_motion: bool,
    /// A connection note (`reconnecting`, `gateway restarting`).
    pub note: Option<String>,
    /// The local runtime's GPU and RAM use while a local model is
    /// resident.
    pub resources: Option<LocalResources>,
    /// The session's live automation rows by kind (`loop · every 5m · 2
    /// runs · next in 4m`), from `automation.state`.
    pub automation: std::collections::BTreeMap<String, String>,
    /// A theme's spinner frames, in place of the indicator style's.
    pub spinner: Vec<String>,
    /// A theme's status phrases, one shown at a time while busy.
    pub phrases: Vec<String>,
}

/// The status item that opens the session switcher when clicked.
#[must_use]
pub fn live_item(strings: &Strings, live: usize) -> String {
    t!(strings, "tui.live", 0 = live)
}

impl Status {
    /// Context use as a whole percentage, when the window is known.
    #[must_use]
    pub fn context_percent(&self) -> Option<u64> {
        let (used, window) = (self.context_tokens?, self.context_window?);
        (window > 0).then(|| (used.saturating_mul(100) / window).min(100))
    }

    /// The items after the battery, in order.
    fn items(&self, strings: &Strings) -> Vec<String> {
        let mut items = vec![self.profile.clone()];
        if let Some(model) = &self.model {
            items.push(model.clone());
        }
        if let Some(percent) = self.context_percent() {
            items.push(t!(strings, "tui.ctx", 0 = percent));
        }
        if let Some(cost) = self.cost_usd {
            items.push(format!("${cost:.4}"));
        }
        if let Some(billing) = &self.billing {
            items.push(billing.clone());
        }
        if let Some(quota) = &self.quota {
            items.push(quota.clone());
        }
        if self.approvals == 1 {
            items.push(t!(strings, "tui.approvals_one", 0 = self.approvals));
        } else if self.approvals > 1 {
            items.push(t!(strings, "tui.approvals", 0 = self.approvals));
        }
        if self.live > 1 {
            items.push(live_item(strings, self.live));
        }
        if let Some(shared) = &self.shared {
            items.push(shared.clone());
        }
        if let Some(note) = &self.note {
            items.push(note.clone());
        }
        if let Some(resources) = &self.resources {
            if let Some(gpu) = resources.gpu() {
                items.push(t!(strings, "tui.gpu", 0 = gpu));
            }
            items.push(t!(strings, "tui.ram", 0 = resources.ram()));
        }
        items.extend(self.automation.values().cloned());
        items
    }

    /// The glyph shown while busy: the theme's spinner frame, else the
    /// indicator style's.
    #[must_use]
    pub fn busy_glyph(&self) -> String {
        if self.spinner.is_empty() {
            return busy::glyph(self.indicator, self.busy_for, self.reduced_motion).to_owned();
        }
        self.spinner[self.step(self.spinner.len())].clone()
    }

    /// The frame or phrase index `busy_for` into a busy period (the first
    /// under reduced motion).
    fn step(&self, len: usize) -> usize {
        if self.reduced_motion || len == 0 {
            return 0;
        }
        let step = self.busy_for.as_millis() / busy::PERIOD.as_millis();
        usize::try_from(step % len as u128).unwrap_or(0)
    }

    /// The lead: the busy glyph (and a theme phrase) and a space, else two
    /// blanks (a narrow terminal keeps its columns for the items while
    /// idle).
    fn lead(&self) -> String {
        if !self.busy {
            return "  ".to_owned();
        }
        match self.phrases.get(self.step(self.phrases.len())) {
            Some(phrase) => format!("{} {phrase} ", self.busy_glyph()),
            None => format!("{} ", self.busy_glyph()),
        }
    }

    /// The items that fit `room` columns, as `(text, is_battery)` spans
    /// with their separators.
    fn fitted(&self, strings: &Strings, room: usize) -> Vec<(String, bool)> {
        let mut spans: Vec<(String, bool)> = Vec::new();
        let mut used = 0;
        let battery = self.battery.map(|b| (b.item(), true));
        let rest = self.items(strings).into_iter().map(|item| (item, false));
        for (item, is_battery) in battery.into_iter().chain(rest) {
            let sep = if spans.is_empty() {
                0
            } else {
                SEPARATOR.width()
            };
            if used + sep + item.width() > room && !spans.is_empty() {
                break;
            }
            if sep > 0 {
                spans.push((SEPARATOR.to_owned(), false));
            }
            used += sep + item.width();
            spans.push((item, is_battery));
        }
        spans
    }

    /// The status line as plain text for `width` columns (the line
    /// interface's bottom line).
    #[must_use]
    pub fn text(&self, width: u16, strings: &Strings) -> String {
        let width = usize::from(width);
        let lead = self.lead();
        let room = width.saturating_sub(lead.width());
        let text: String = self
            .fitted(strings, room)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        truncate(&format!("{lead}{}", truncate(&text, room)), width)
    }

    /// The status line for `width` columns.
    #[must_use]
    pub fn line(&self, width: u16, theme: &Theme, strings: &Strings) -> Line<'static> {
        let width = usize::from(width);
        let lead = self.lead();
        let room = width.saturating_sub(lead.width());
        let mut spans = vec![Span::styled(lead, theme.status.patch(theme.busy))];
        let mut used = 0;
        for (text, is_battery) in self.fitted(strings, room) {
            let text = truncate(&text, room - used);
            used += text.width();
            let style: Style = match self.battery {
                Some(battery) if is_battery => theme.status.patch(theme.battery(battery.level())),
                _ => theme.status,
            };
            spans.push(Span::styled(text, style));
        }
        spans.push(Span::styled(
            " ".repeat(room.saturating_sub(used)),
            theme.status,
        ));
        Line::from(spans)
    }

    /// The column range of the `<n> live` item on a line of `width`, for
    /// a click on it.
    #[must_use]
    pub fn live_columns(&self, width: u16, strings: &Strings) -> Option<std::ops::Range<u16>> {
        let wanted = live_item(strings, self.live);
        let lead = self.lead().width();
        let room = usize::from(width).saturating_sub(lead);
        let mut at = lead;
        for (text, _) in self.fitted(strings, room) {
            if text == wanted {
                let start = u16::try_from(at).ok()?;
                let end = u16::try_from(at + text.width()).ok()?;
                return Some(start..end);
            }
            at += text.width();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pmt_config::schema::{BusyIndicator, TuiTheme};

    use super::{LocalResources, Status, Strings};
    use crate::theme::Theme;
    use pmt_core::battery::Battery;

    fn text(status: &Status, width: u16) -> String {
        let line = status.line(
            width,
            &Theme::new(TuiTheme::Dark, true),
            &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui),
        );
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn full() -> Status {
        Status {
            profile: "work".into(),
            model: Some("mock/m1".into()),
            context_tokens: Some(1500),
            context_window: Some(8000),
            cost_usd: Some(0.0123),
            approvals: 2,
            indicator: BusyIndicator::Ascii,
            ..Status::default()
        }
    }

    #[test]
    fn automation_rows_show_after_the_other_items() {
        let mut status = full();
        status.automation.insert(
            "loop".into(),
            "loop · every 5m · 2 runs · next in 4m".into(),
        );
        let shown = text(&status, 120);
        assert!(
            shown.contains("2 approvals │ loop · every 5m · 2 runs · next in 4m"),
            "{shown}"
        );
    }

    #[test]
    fn every_item_shows_when_there_is_room() {
        assert_eq!(
            text(&full(), 70).trim_end(),
            "  work │ mock/m1 │ ctx 18% │ $0.0123 │ 2 approvals"
        );
        assert_eq!(
            text(&full(), 70).trim_end(),
            full().text(70, &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui))
        );
    }

    #[test]
    fn a_narrow_line_drops_items_from_the_end() {
        assert_eq!(text(&full(), 30).trim_end(), "  work │ mock/m1 │ ctx 18%");
        assert_eq!(text(&full(), 30).chars().count(), 30, "padded to the width");
    }

    #[test]
    fn reduced_motion_shows_a_still_glyph() {
        let mut status = full();
        status.busy = true;
        status.busy_for = Duration::from_secs(3);
        assert!(text(&status, 70).starts_with("/ "));
        status.reduced_motion = true;
        assert!(text(&status, 70).starts_with("| "));
    }

    #[test]
    fn the_battery_leads_and_live_sessions_follow_the_approvals() {
        let mut status = full();
        status.battery = Some(Battery {
            percent: 9,
            charging: false,
        });
        status.live = 2;
        assert_eq!(
            text(&status, 90).trim_end(),
            "  ▮ 9% │ work │ mock/m1 │ ctx 18% │ $0.0123 │ 2 approvals │ 2 live"
        );
        let columns = status
            .live_columns(90, &pmt_i18n::Strings::english(pmt_i18n::Surface::Tui))
            .unwrap();
        let shown = text(&status, 90);
        let picked: String = shown
            .chars()
            .skip(usize::from(columns.start))
            .take(columns.len())
            .collect();
        assert_eq!(picked, "2 live");
    }

    /// Local resource telemetry: GPU utilisation, GPU memory and RAM of
    /// the local runtime show while a local model is resident.
    #[test]
    fn local_resources_show_gpu_and_ram_use() {
        const GIB: u64 = 1 << 30;
        let mut status = full();
        status.approvals = 0;
        status.resources = Some(LocalResources {
            gpu_percent: Some(37.0),
            gpu_used: 2 * GIB,
            gpu_total: 24 * GIB,
            ram_used: 16 * GIB,
            ram_total: 32 * GIB,
        });
        assert_eq!(
            text(&status, 100).trim_end(),
            "  work │ mock/m1 │ ctx 18% │ $0.0123 │ GPU 37% 2.0/24.0 GiB │ RAM 16.0/32.0 GiB"
        );
        // No GPU: RAM alone; no utilisation reading: memory alone.
        status.resources = Some(LocalResources {
            gpu_total: 0,
            ..status.resources.unwrap()
        });
        assert!(
            text(&status, 100)
                .trim_end()
                .ends_with("$0.0123 │ RAM 16.0/32.0 GiB")
        );
        status.resources = Some(LocalResources {
            gpu_percent: None,
            gpu_used: GIB,
            gpu_total: 8 * GIB,
            ram_used: 0,
            ram_total: 0,
        });
        assert!(text(&status, 100).contains("│ GPU 1.0/8.0 GiB │"));
    }

    /// Scenario: Static UI string catalogs (the status line in a bundled
    /// non-English language).
    #[test]
    fn the_status_line_speaks_the_profile_language() {
        let dir = tempfile::tempdir().unwrap();
        let sources = pmt_i18n::Sources::of_profile("de", dir.path(), Vec::new());
        let german = Strings::load(&sources, pmt_i18n::Surface::Tui);
        let mut status = full();
        status.live = 2;
        let shown = status.text(120, &german);
        assert!(shown.contains("2 Genehmigungen"), "{shown}");
        assert!(shown.contains("2 aktiv"), "{shown}");
        assert!(!shown.contains("approvals"), "{shown}");
    }
}

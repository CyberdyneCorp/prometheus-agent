//! The TUI's display commands (spec: tui, Client-local display commands;
//! Busy indicator styles; Terminal background detection; Collapsible
//! startup banner): `/redraw`, `/statusbar`, `/battery`, `/indicator` and
//! `/theme` change only this screen (the toggles persist through the
//! profile's config writer and never reach the daemon), and the startup
//! banner is filled from `session.banner`.

use pmt_config::schema::TuiTheme;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::{BannerInfo, SessionBanner, SessionRef};
use pmt_tui::banner::{Banner, Contents};
use pmt_tui::busy;
use pmt_tui::theme::Theme;

use super::screen::{Done, Tui};

impl Tui<'_> {
    /// Shows a command's result.
    fn report(&mut self, result: Result<String, Error>) {
        match result {
            Ok(text) if text.is_empty() => {}
            Ok(text) => self.notice(text),
            Err(err) => self.error(&err),
        }
    }

    /// `/redraw`: the next frame clears the terminal and paints it whole.
    pub(super) fn redraw(&mut self) {
        self.clear_next = true;
    }

    /// `/statusbar [on|off]` (`/sb`).
    pub(super) fn statusbar(&mut self, args: &str) {
        let result = self.status_bar.statusbar(self.open.scope, args);
        self.app.status_visible = self.status_bar.shown;
        self.report(result);
    }

    /// `/battery [on|off|status]`.
    pub(super) fn battery(&mut self, args: &str) {
        let result = self.status_bar.battery_command(self.open.scope, args);
        self.report(result);
    }

    /// `/indicator [style]`: shows or sets `ui.tui.busy_indicator`.
    pub(super) fn indicator(&mut self, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            let current = busy::name(self.app.status.indicator);
            self.notice(format!(
                "busy indicator: {current} ({})",
                busy::NAMES.join(", ")
            ));
            return;
        }
        let result = busy::parse(name)
            .map_err(|message| Error::new(&codes::CLI_USAGE, message))
            .and_then(|style| {
                self.open
                    .scope
                    .profile()
                    .config_file()
                    .set("ui.tui.busy_indicator", name)?;
                self.app.status.indicator = style;
                Ok(format!("busy indicator: {name} (ui.tui.busy_indicator)"))
            });
        self.report(result);
    }

    /// `/theme [dark|light]`: shows the theme in use (and where it came
    /// from) or sets one for this run.
    pub(super) fn theme(&mut self, args: &str) {
        let theme = match args.trim() {
            "" => {
                let configured = self.open.scope.config().ui.tui.theme;
                let source = if configured == TuiTheme::Auto {
                    " (auto: detected)"
                } else {
                    " (ui.tui.theme)"
                };
                let colour = if self.app.theme.colored() {
                    ""
                } else {
                    ", no colour"
                };
                let name = theme_name(self.theme_in_use);
                self.notice(format!("theme: {name}{source}{colour}"));
                return;
            }
            "dark" => TuiTheme::Dark,
            "light" => TuiTheme::Light,
            other => {
                let err = Error::new(
                    &codes::CLI_USAGE,
                    format!("unknown theme '{other}' (dark, light)"),
                );
                self.error(&err);
                return;
            }
        };
        self.theme_in_use = theme;
        self.app.theme = Theme::new(theme, self.open.no_color);
    }

    /// Asks for the banner's contents in the background.
    pub(super) fn load_banner(&self) {
        let params = SessionRef {
            session_id: self.link.session.clone(),
        };
        let client = self.link.client.clone();
        self.background(async move {
            Done::Banner(
                client
                    .call_typed::<SessionBanner>(&params)
                    .await
                    .unwrap_or_default(),
            )
        });
    }

    /// Puts the banner above the transcript, its toggles at their
    /// defaults.
    pub(super) fn show_banner(&mut self, info: BannerInfo) {
        let model = self.app.status.model.clone().unwrap_or_default();
        let title = format!(
            "Prometheus {} · {} · {model}",
            crate::build_info::VERSION,
            self.app.status.profile
        );
        let system_prompt = info.system_prompt.map_or_else(
            || vec!["(built on the first turn)".to_owned()],
            |prompt| {
                std::iter::once(prompt.sha256)
                    .chain(prompt.text.lines().map(str::to_owned))
                    .collect()
            },
        );
        let contents = Contents {
            tools: info.tools,
            skills: info.skills,
            system_prompt,
            mcp_servers: info
                .mcp_servers
                .into_iter()
                .map(|s| format!("{} ({}, {} tools)", s.name, s.state, s.tools))
                .collect(),
        };
        self.app.banner = Some(Banner::new(title, contents));
    }
}

const fn theme_name(theme: TuiTheme) -> &'static str {
    match theme {
        TuiTheme::Light => "light",
        TuiTheme::Dark | TuiTheme::Auto => "dark",
    }
}

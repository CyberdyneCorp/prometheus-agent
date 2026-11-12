//! What a terminal chat shows and how (spec: cli, Display preferences;
//! Themes are data): the theme, colour, tool progress mode, focus and
//! timestamps, and the commands that change them (`/theme`, `/verbose`,
//! `/focus`, `/timestamps`). The line interface and the chat attached to
//! a daemon share it; the session never sees it.

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_config::schema::ToolProgress;
use pmt_core::error::Error;
use pmt_i18n::t;

use super::theme::{self, Role, Theme};

/// Display state the user can change in the session.
pub struct Display {
    /// The colours.
    pub theme: Theme,
    /// Colour is on.
    pub color: bool,
    /// `-q`.
    pub quiet: bool,
    /// Tool progress mode (`/verbose`).
    pub progress: ToolProgress,
    /// `/focus on`: the mode `/focus off` restores.
    pub focus: Option<ToolProgress>,
    /// `display.timestamps` (`/timestamps`).
    pub timestamps: bool,
}

/// The name of a progress mode.
#[must_use]
pub fn mode_name(mode: ToolProgress) -> &'static str {
    match mode {
        ToolProgress::Off => "off",
        ToolProgress::New => "new",
        ToolProgress::All => "all",
        ToolProgress::Verbose => "verbose",
    }
}

impl Display {
    /// The display `scope`'s preferences ask for (`no_color`: `--no-color`
    /// or `NO_COLOR`).
    ///
    /// # Panics
    ///
    /// Never: the `default` theme is built in.
    #[must_use]
    pub fn new(scope: &ProfileScope, no_color: bool, quiet: bool) -> Self {
        use std::io::IsTerminal as _;
        let config = &scope.config().display;
        let theme = Theme::load(scope.dir(), &config.theme).unwrap_or_else(|why| {
            tracing::warn!(theme = %config.theme, %why, "theme unavailable; using default");
            Theme::load(scope.dir(), "default").expect("built-in theme")
        });
        Self {
            theme,
            color: theme::color_enabled(no_color, false, std::io::stdout().is_terminal()),
            quiet,
            progress: config.tool_progress,
            focus: None,
            timestamps: config.timestamps,
        }
    }

    /// `text` in `role`'s colour.
    #[must_use]
    pub fn paint(&self, role: Role, text: &str) -> String {
        self.theme.paint(role, text, self.color)
    }

    /// `/theme [name]`: lists the themes, or switches to one and saves it
    /// in `display.theme`.
    ///
    /// # Errors
    ///
    /// The config write fails.
    pub fn set_theme(&mut self, scope: &ProfileScope, args: &str) -> Result<String, Error> {
        let dir = scope.dir();
        let strings = pmt_config::language::strings(dir, pmt_i18n::Surface::Core);
        let available = theme::available(dir);
        if args.is_empty() || !available.iter().any(|t| t == args) {
            let mut text = String::new();
            if !args.is_empty() {
                let _ = writeln!(text, "{}", t!(strings, "display.no_theme", name = args));
            }
            text.push_str(strings.get("display.themes"));
            for name in &available {
                let _ = write!(text, "\n  {name}");
                if *name == self.theme.name {
                    let _ = write!(text, " {}", strings.get("display.current_mark"));
                }
            }
            return Ok(text);
        }
        match Theme::load(dir, args) {
            Ok(theme) => {
                self.theme = theme;
                scope.profile().config_file().set("display.theme", args)?;
                Ok(t!(strings, "display.theme_set", name = args))
            }
            Err(why) => Ok(why),
        }
    }

    /// `/focus [on|off|status]`.
    pub fn focus(&mut self, args: &str, strings: &pmt_i18n::Strings) -> String {
        match args.trim() {
            "on" => {
                if self.focus.is_none() {
                    self.focus = Some(self.progress);
                    self.progress = ToolProgress::Off;
                }
                t!(strings, "display.focus_on")
            }
            "off" => match self.focus.take() {
                Some(previous) => {
                    self.progress = previous;
                    t!(strings, "display.focus_off", mode = mode_name(previous))
                }
                None => t!(strings, "display.focus_is_off"),
            },
            _ => match self.focus {
                Some(_) => t!(strings, "display.focus_is_on"),
                None => t!(
                    strings,
                    "display.focus_status_off",
                    mode = mode_name(self.progress)
                ),
            },
        }
    }

    /// `/verbose`: the next progress mode (the one focus restores while
    /// focus is on).
    pub fn verbose(&mut self, strings: &pmt_i18n::Strings) -> String {
        let slot = match &mut self.focus {
            Some(saved) => saved,
            None => &mut self.progress,
        };
        *slot = match *slot {
            ToolProgress::Off => ToolProgress::New,
            ToolProgress::New => ToolProgress::All,
            ToolProgress::All => ToolProgress::Verbose,
            ToolProgress::Verbose => ToolProgress::Off,
        };
        t!(strings, "display.tool_progress", mode = mode_name(*slot))
    }

    /// `/timestamps [on|off|status]` (alias `/ts`), saved in
    /// `display.timestamps`.
    ///
    /// # Errors
    ///
    /// The config write fails.
    pub fn set_timestamps(&mut self, scope: &ProfileScope, args: &str) -> Result<String, Error> {
        let strings = pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core);
        let state = |on: bool| strings.get(if on { "cli.on" } else { "cli.off" });
        let on = match args.trim() {
            "on" => true,
            "off" => false,
            _ => {
                let state = state(self.timestamps);
                return Ok(t!(strings, "display.timestamps_are", state = state));
            }
        };
        scope
            .profile()
            .config_file()
            .set("display.timestamps", if on { "true" } else { "false" })?;
        self.timestamps = on;
        Ok(t!(strings, "display.timestamps_set", state = state(on)))
    }

    /// `text` with the time prefix when timestamps are on.
    #[must_use]
    pub fn stamped(&self, scope: &ProfileScope, text: &str) -> String {
        if !self.timestamps {
            return text.to_owned();
        }
        let format = &scope.config().display.timestamp_format;
        let now = jiff::Zoned::now();
        format!("[{}] {text}", now.strftime(format.as_str()))
    }
}

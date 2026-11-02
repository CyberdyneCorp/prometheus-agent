//! What a terminal chat shows and how (spec: cli, Display preferences;
//! Themes are data): the theme, colour, tool progress mode, focus and
//! timestamps, and the commands that change them (`/theme`, `/verbose`,
//! `/focus`, `/timestamps`). The line interface and the chat attached to
//! a daemon share it; the session never sees it.

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_config::schema::ToolProgress;
use pmt_core::error::Error;

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
        let available = theme::available(dir);
        if args.is_empty() || !available.iter().any(|t| t == args) {
            let mut text = String::new();
            if !args.is_empty() {
                let _ = writeln!(text, "no theme '{args}'");
            }
            let _ = write!(text, "themes:");
            for name in &available {
                let mark = if *name == self.theme.name {
                    " (current)"
                } else {
                    ""
                };
                let _ = write!(text, "\n  {name}{mark}");
            }
            return Ok(text);
        }
        match Theme::load(dir, args) {
            Ok(theme) => {
                self.theme = theme;
                scope.profile().config_file().set("display.theme", args)?;
                Ok(format!("theme {args}"))
            }
            Err(why) => Ok(why),
        }
    }

    /// `/focus [on|off|status]`.
    pub fn focus(&mut self, args: &str) -> String {
        match args.trim() {
            "on" => {
                if self.focus.is_none() {
                    self.focus = Some(self.progress);
                    self.progress = ToolProgress::Off;
                }
                "focus on: tool progress hidden".to_owned()
            }
            "off" => match self.focus.take() {
                Some(previous) => {
                    self.progress = previous;
                    format!("focus off: tool progress {}", mode_name(previous))
                }
                None => "focus is off".to_owned(),
            },
            _ => match self.focus {
                Some(_) => "focus is on".to_owned(),
                None => format!("focus is off (tool progress {})", mode_name(self.progress)),
            },
        }
    }

    /// `/verbose`: the next progress mode (the one focus restores while
    /// focus is on).
    pub fn verbose(&mut self) -> String {
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
        format!("tool progress: {}", mode_name(*slot))
    }

    /// `/timestamps [on|off|status]` (alias `/ts`), saved in
    /// `display.timestamps`.
    ///
    /// # Errors
    ///
    /// The config write fails.
    pub fn set_timestamps(&mut self, scope: &ProfileScope, args: &str) -> Result<String, Error> {
        let on = match args.trim() {
            "on" => true,
            "off" => false,
            _ => {
                let state = if self.timestamps { "on" } else { "off" };
                return Ok(format!("timestamps are {state}"));
            }
        };
        scope
            .profile()
            .config_file()
            .set("display.timestamps", if on { "true" } else { "false" })?;
        self.timestamps = on;
        Ok(format!("timestamps {}", if on { "on" } else { "off" }))
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

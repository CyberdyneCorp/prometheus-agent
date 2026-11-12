//! What `/` input means in the TUI (spec: tui, Slash command flow): a
//! command the TUI handles itself (no round-trip), a command the daemon
//! runs (`command.dispatch`), or, when its first word is no registered
//! command (`/etc/hosts`, `/var/log/syslog is huge`), a prompt.

/// A command the TUI runs itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// Leave the TUI (the session keeps running in the daemon).
    Quit,
    /// Clear the screen's transcript (the session is unchanged).
    Clear,
    /// Copy the last answer to the clipboard (OSC 52).
    Copy,
    /// Show or set the colour theme for this run.
    Theme,
    /// List the effective key bindings.
    Keys,
    /// Leave the TUI and update the binary (`prometheus update`).
    Update,
    /// Start a new session and switch to it.
    New,
    /// Switch to another session (`/resume <id|title>`).
    Resume,
    /// Fork the session at its last message and switch to the fork.
    Fork,
    /// Clear and fully repaint the screen.
    Redraw,
    /// Show or hide the status bar (`ui.status_bar`).
    StatusBar,
    /// Show, hide or report the battery item (`ui.status_bar_battery`).
    Battery,
    /// Show or set the busy indicator style (`ui.tui.busy_indicator`).
    Indicator,
    /// Open the command palette.
    Palette,
    /// Open the live session switcher (`/sessions new` starts one).
    Sessions,
    /// Attach the clipboard's image to the next prompt.
    Paste,
    /// Make `Ctrl+V` reach the TUI in VS Code, Cursor or Windsurf.
    TerminalSetup,
    /// Continue the session in a chat and leave (`/handoff <platform>`).
    Handoff,
    /// Voice mode: `/voice [on|off|tts|status]` (recorded and spoken here).
    Voice,
}

/// One client-local command: its name, other names and `/help` line.
#[derive(Debug, Clone, Copy)]
pub struct LocalCommand {
    /// The command.
    pub command: Local,
    /// Its name without `/`.
    pub name: &'static str,
    /// Other names.
    pub aliases: &'static [&'static str],
    /// The catalog key (`tui` surface) of its one-line description.
    pub key: &'static str,
}

impl LocalCommand {
    /// Its one-line description in the language of `strings`.
    #[must_use]
    pub fn description<'a>(&self, strings: &'a pmt_i18n::Strings) -> &'a str {
        strings.get(self.key)
    }
}

/// The TUI's own commands.
pub const LOCAL: &[LocalCommand] = &[
    LocalCommand {
        command: Local::Quit,
        name: "quit",
        aliases: &["exit", "q"],
        key: "tui.cmd_quit",
    },
    LocalCommand {
        command: Local::Clear,
        name: "clear",
        aliases: &[],
        key: "tui.cmd_clear",
    },
    LocalCommand {
        command: Local::Copy,
        name: "copy",
        aliases: &[],
        key: "tui.cmd_copy",
    },
    LocalCommand {
        command: Local::Theme,
        name: "theme",
        aliases: &[],
        key: "tui.cmd_theme",
    },
    LocalCommand {
        command: Local::Keys,
        name: "keys",
        aliases: &[],
        key: "tui.cmd_keys",
    },
    LocalCommand {
        command: Local::Update,
        name: "update",
        aliases: &[],
        key: "tui.cmd_update",
    },
    LocalCommand {
        command: Local::New,
        name: "new",
        aliases: &["reset"],
        key: "tui.cmd_new",
    },
    LocalCommand {
        command: Local::Resume,
        name: "resume",
        aliases: &[],
        key: "tui.cmd_resume",
    },
    LocalCommand {
        command: Local::Fork,
        name: "fork",
        aliases: &["branch"],
        key: "tui.cmd_fork",
    },
    LocalCommand {
        command: Local::Redraw,
        name: "redraw",
        aliases: &[],
        key: "tui.cmd_redraw",
    },
    LocalCommand {
        command: Local::StatusBar,
        name: "statusbar",
        aliases: &["sb"],
        key: "tui.cmd_statusbar",
    },
    LocalCommand {
        command: Local::Battery,
        name: "battery",
        aliases: &[],
        key: "tui.cmd_battery",
    },
    LocalCommand {
        command: Local::Indicator,
        name: "indicator",
        aliases: &[],
        key: "tui.cmd_indicator",
    },
    LocalCommand {
        command: Local::Palette,
        name: "palette",
        aliases: &[],
        key: "tui.cmd_palette",
    },
    LocalCommand {
        command: Local::Sessions,
        name: "sessions",
        aliases: &["switch"],
        key: "tui.cmd_sessions",
    },
    LocalCommand {
        command: Local::Paste,
        name: "paste",
        aliases: &[],
        key: "tui.cmd_paste",
    },
    LocalCommand {
        command: Local::TerminalSetup,
        name: "terminal-setup",
        aliases: &[],
        key: "tui.cmd_terminal_setup",
    },
    LocalCommand {
        command: Local::Handoff,
        name: "handoff",
        aliases: &[],
        key: "tui.cmd_handoff",
    },
    LocalCommand {
        command: Local::Voice,
        name: "voice",
        aliases: &[],
        key: "tui.cmd_voice",
    },
];

/// What a submitted line is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A client-local command and its arguments.
    Local(Local, String),
    /// A command for `command.dispatch`.
    Dispatch {
        /// Its name without `/`.
        name: String,
        /// The rest of the line.
        args: String,
    },
    /// Text to submit as a prompt.
    Prompt(String),
}

/// The local command named `name` (or one of its aliases).
#[must_use]
pub fn local(name: &str) -> Option<Local> {
    LOCAL
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
        .map(|c| c.command)
}

/// Classifies `line`; `registered` says whether the daemon's catalog has a
/// command of that name or alias.
#[must_use]
pub fn classify(line: &str, registered: impl Fn(&str) -> bool) -> Input {
    let Some(rest) = line.strip_prefix('/') else {
        return Input::Prompt(line.to_owned());
    };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim().to_owned()),
        None => (rest, String::new()),
    };
    if let Some(command) = local(name) {
        return Input::Local(command, args);
    }
    if !name.is_empty() && registered(name) {
        return Input::Dispatch {
            name: name.to_owned(),
            args,
        };
    }
    Input::Prompt(line.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Input, Local, classify};

    fn registered(name: &str) -> bool {
        ["deploy", "compress", "model"].contains(&name)
    }

    #[test]
    fn local_commands_never_leave_the_client() {
        assert_eq!(
            classify("/quit", registered),
            Input::Local(Local::Quit, String::new())
        );
        assert_eq!(
            classify("/theme light", registered),
            Input::Local(Local::Theme, "light".into())
        );
    }

    #[test]
    fn registered_commands_are_dispatched_with_their_arguments() {
        assert_eq!(
            classify("/deploy staging", registered),
            Input::Dispatch {
                name: "deploy".into(),
                args: "staging".into()
            }
        );
    }

    #[test]
    fn a_path_or_unknown_word_is_a_prompt() {
        for line in ["/var/log/syslog is huge", "/etc/hosts", "/", "hello /model"] {
            assert_eq!(
                classify(line, registered),
                Input::Prompt(line.into()),
                "{line}"
            );
        }
    }
}

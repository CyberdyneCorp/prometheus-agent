//! Which protocol a terminal speaks, from its environment only (a query
//! answered on stdin could hang a pipe or be swallowed by a key reader).

use pmt_config::schema::PetRenderMode;

use super::Mode;

/// The environment variables detection reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalEnv {
    /// `TERM`.
    pub term: String,
    /// `TERM_PROGRAM`.
    pub term_program: String,
    /// `KITTY_WINDOW_ID` is set.
    pub kitty_window: bool,
    /// `ITERM_SESSION_ID` is set.
    pub iterm_session: bool,
    /// `WEZTERM_PANE` is set.
    pub wezterm_pane: bool,
}

impl TerminalEnv {
    /// Reads them through `get` (the launch environment).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| get(name).is_some_and(|v| !v.is_empty());
        Self {
            term: get("TERM").unwrap_or_default().to_lowercase(),
            term_program: get("TERM_PROGRAM").unwrap_or_default().to_lowercase(),
            kitty_window: set("KITTY_WINDOW_ID"),
            iterm_session: set("ITERM_SESSION_ID"),
            wezterm_pane: set("WEZTERM_PANE"),
        }
    }
}

/// The richest protocol the terminal is known to speak.
#[must_use]
pub fn detect(env: &TerminalEnv) -> Mode {
    let term = env.term.as_str();
    let program = env.term_program.as_str();
    // Editor terminals inherit the host's variables but render inline
    // images only when the user opted in: half-blocks always work there.
    if program == "vscode" {
        return Mode::Unicode;
    }
    let wezterm = program == "wezterm" || env.wezterm_pane;
    if env.kitty_window
        || term.contains("kitty")
        || term.contains("ghostty")
        || program == "ghostty"
        || wezterm
    {
        return Mode::Kitty;
    }
    if program == "iterm.app" || env.iterm_session {
        return Mode::Iterm;
    }
    if program == "mintty" || ["foot", "mlterm", "sixel"].iter().any(|t| term.contains(t)) {
        return Mode::Sixel;
    }
    Mode::Unicode
}

/// The mode a surface draws in: off when the output is not a terminal,
/// else the configured mode (`auto`: [`detect`]).
#[must_use]
pub fn resolve(configured: PetRenderMode, is_tty: bool, env: &TerminalEnv) -> Mode {
    if !is_tty {
        return Mode::Off;
    }
    match configured {
        PetRenderMode::Auto => detect(env),
        PetRenderMode::Kitty => Mode::Kitty,
        PetRenderMode::Iterm => Mode::Iterm,
        PetRenderMode::Sixel => Mode::Sixel,
        PetRenderMode::Unicode => Mode::Unicode,
        PetRenderMode::Off => Mode::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(term: &str, program: &str) -> TerminalEnv {
        TerminalEnv {
            term: term.into(),
            term_program: program.into(),
            ..TerminalEnv::default()
        }
    }

    #[test]
    fn terminals_are_detected_from_their_variables() {
        assert_eq!(detect(&env("xterm-kitty", "")), Mode::Kitty);
        assert_eq!(detect(&env("xterm-256color", "ghostty")), Mode::Kitty);
        assert_eq!(detect(&env("xterm-256color", "iterm.app")), Mode::Iterm);
        assert_eq!(detect(&env("foot", "")), Mode::Sixel);
        assert_eq!(detect(&env("xterm-256color", "")), Mode::Unicode);
        let vscode = TerminalEnv {
            iterm_session: true,
            ..env("xterm-256color", "vscode")
        };
        assert_eq!(detect(&vscode), Mode::Unicode);
    }

    #[test]
    fn redirected_output_is_always_off() {
        let kitty = env("xterm-kitty", "");
        assert_eq!(resolve(PetRenderMode::Kitty, false, &kitty), Mode::Off);
        assert_eq!(resolve(PetRenderMode::Auto, false, &kitty), Mode::Off);
        assert_eq!(resolve(PetRenderMode::Auto, true, &kitty), Mode::Kitty);
        assert_eq!(resolve(PetRenderMode::Off, true, &kitty), Mode::Off);
    }
}

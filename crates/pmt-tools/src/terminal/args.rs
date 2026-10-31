//! The `terminal` call arguments and their validation, done before anything
//! runs (spec: execution-environments, Terminal argument validation).

use serde::Deserialize;

/// One `terminal` call as the model sends it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalArgs {
    /// The shell command line.
    pub command: String,
    /// Run as a tracked background process.
    pub background: bool,
    /// Seconds before a foreground command is stopped (default
    /// `terminal.timeout_secs`).
    pub timeout: Option<f64>,
    /// Directory for this call only; never moves the session cwd.
    pub workdir: Option<String>,
    /// Allocate a pseudo-terminal (background only).
    pub pty: bool,
    /// Notify the session when the background process finishes.
    pub notify_on_complete: bool,
    /// Output patterns whose matches are reported (background only).
    pub watch_patterns: Vec<String>,
    /// Seconds between progress notifications (background only).
    pub heartbeat: Option<f64>,
}

impl TerminalArgs {
    /// A foreground call of `command` with every other argument unset.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Self::default()
        }
    }

    /// Checks the argument combination.
    ///
    /// # Errors
    ///
    /// The message the result carries, for example `pty requires
    /// background=true` or `timeout must be a positive number of seconds
    /// (got 0)`.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(timeout) = self.timeout
            && !(timeout > 0.0 && timeout.is_finite())
        {
            return Err(format!(
                "timeout must be a positive number of seconds (got {})",
                number(timeout)
            ));
        }
        if let Some(heartbeat) = self.heartbeat
            && (heartbeat < 0.0 || heartbeat.is_nan())
        {
            return Err(format!(
                "heartbeat must not be negative (got {})",
                number(heartbeat)
            ));
        }
        if self.background {
            return Ok(());
        }
        if self.pty {
            return Err("pty requires background=true".to_owned());
        }
        let foreground_only = [
            (self.notify_on_complete, "notify_on_complete"),
            (!self.watch_patterns.is_empty(), "watch_patterns"),
            (self.heartbeat.is_some_and(|h| h > 0.0), "heartbeat"),
        ];
        match foreground_only.iter().find(|(set, _)| *set) {
            Some((_, name)) => Err(format!("{name} requires background=true")),
            None => Ok(()),
        }
    }
}

/// `n` as the model wrote it: `0`, `-5`, `1.5`.
fn number(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e15 {
        format!("{n:.0}")
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::TerminalArgs;

    fn check(args: &TerminalArgs) -> Result<(), String> {
        args.validate()
    }

    #[test]
    fn pty_in_foreground_is_refused() {
        let args = TerminalArgs {
            pty: true,
            ..TerminalArgs::new("python")
        };
        assert!(
            check(&args)
                .unwrap_err()
                .starts_with("pty requires background=true")
        );
    }

    #[test]
    fn background_only_options_are_refused_in_foreground() {
        let notify = TerminalArgs {
            notify_on_complete: true,
            ..TerminalArgs::new("make")
        };
        assert_eq!(
            check(&notify).unwrap_err(),
            "notify_on_complete requires background=true"
        );
        let watch = TerminalArgs {
            watch_patterns: vec!["ready".into()],
            ..TerminalArgs::new("make")
        };
        assert_eq!(
            check(&watch).unwrap_err(),
            "watch_patterns requires background=true"
        );
        let heartbeat = TerminalArgs {
            heartbeat: Some(5.0),
            ..TerminalArgs::new("make")
        };
        assert_eq!(
            check(&heartbeat).unwrap_err(),
            "heartbeat requires background=true"
        );
        let zero = TerminalArgs {
            heartbeat: Some(0.0),
            ..TerminalArgs::new("make")
        };
        assert!(check(&zero).is_ok());
    }

    #[test]
    fn timeouts_must_be_positive_and_heartbeats_not_negative() {
        for (timeout, shown) in [(0.0, "0"), (-5.0, "-5"), (-0.5, "-0.5")] {
            let args = TerminalArgs {
                timeout: Some(timeout),
                ..TerminalArgs::new("ls")
            };
            assert_eq!(
                check(&args).unwrap_err(),
                format!("timeout must be a positive number of seconds (got {shown})")
            );
        }
        let args = TerminalArgs {
            background: true,
            heartbeat: Some(-1.0),
            ..TerminalArgs::new("ls")
        };
        assert_eq!(
            check(&args).unwrap_err(),
            "heartbeat must not be negative (got -1)"
        );
        let ok = TerminalArgs {
            background: true,
            pty: true,
            notify_on_complete: true,
            heartbeat: Some(10.0),
            ..TerminalArgs::new("ls")
        };
        assert!(check(&ok).is_ok());
    }
}

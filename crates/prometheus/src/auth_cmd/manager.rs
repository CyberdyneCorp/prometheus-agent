//! `prometheus auth` without a subcommand (spec: model-providers,
//! Credential pool maintenance commands): on a terminal, a small line
//! manager over the maintenance commands; elsewhere the `auth list` peek.

use std::io::{BufRead, Write};

use pmt_core::error::Error;

use crate::context::Context;

const HELP: &str = "commands: list | status <provider> | reset <provider> [label|index] | refresh <provider> [label|index] | priority <provider> <label|index> <position> | logout <provider> | remove <provider> <label|index> | quit";

/// One manager command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    List,
    Status(String),
    Reset(String, Option<String>),
    Refresh(String, Option<String>),
    Priority(String, String, usize),
    Logout(String),
    Remove(String, String),
    Quit,
}

/// Parses one manager line; `Err` holds the usage text to show.
pub(super) fn parse(line: &str) -> Result<Option<Action>, &'static str> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let owned = |w: &str| w.to_owned();
    Ok(Some(match words.as_slice() {
        [] => return Ok(None),
        ["list"] => Action::List,
        ["status", p] => Action::Status(owned(p)),
        ["reset", p] => Action::Reset(owned(p), None),
        ["reset", p, c] => Action::Reset(owned(p), Some(owned(c))),
        ["refresh", p] => Action::Refresh(owned(p), None),
        ["refresh", p, c] => Action::Refresh(owned(p), Some(owned(c))),
        ["priority", p, c, n] => Action::Priority(owned(p), owned(c), n.parse().map_err(|_| HELP)?),
        ["logout", p] => Action::Logout(owned(p)),
        ["remove", p, c] => Action::Remove(owned(p), owned(c)),
        ["quit" | "q" | "exit"] => Action::Quit,
        _ => return Err(HELP),
    }))
}

/// Runs the manager on a terminal, else lists the credentials.
pub(super) fn run(ctx: &Context<'_>) -> Result<u8, Error> {
    if !(crate::prompt::stdin_is_tty() && crate::prompt::stdout_is_tty()) {
        return super::list::list(ctx);
    }
    super::list::list(ctx)?;
    let stdin = std::io::stdin();
    session(ctx, &mut stdin.lock(), &mut std::io::stderr())
}

/// Reads commands from `input` until `quit` or end of input; a failing
/// command is reported and the manager goes on.
fn session(ctx: &Context<'_>, input: &mut impl BufRead, out: &mut impl Write) -> Result<u8, Error> {
    let _ = writeln!(out, "{HELP}");
    loop {
        let _ = write!(out, "auth> ");
        let _ = out.flush();
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .map_err(|e| crate::prompt::read_error(&e))?
            == 0
        {
            return Ok(0);
        }
        let action = match parse(&line) {
            Ok(Some(Action::Quit)) => return Ok(0),
            Ok(Some(action)) => action,
            Ok(None) => continue,
            Err(help) => {
                let _ = writeln!(out, "{help}");
                continue;
            }
        };
        if let Err(err) = apply(ctx, &action) {
            let _ = writeln!(out, "error: {}", err.message());
        }
    }
}

fn apply(ctx: &Context<'_>, action: &Action) -> Result<u8, Error> {
    match action {
        Action::List => super::list::list(ctx),
        Action::Status(p) => super::list::status(ctx, p),
        Action::Reset(p, c) => super::maintain::reset(ctx, p, c.as_deref()),
        Action::Refresh(p, c) => super::maintain::refresh(ctx, p, c.as_deref()),
        Action::Priority(p, c, n) => super::maintain::priority(ctx, p, c, *n),
        Action::Logout(p) => super::maintain::logout(ctx, p),
        Action::Remove(p, c) => super::remove::run(ctx, p, c),
        Action::Quit => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, parse};

    #[test]
    fn manager_lines_map_to_the_maintenance_commands() {
        assert_eq!(parse("  "), Ok(None));
        assert_eq!(
            parse("reset openrouter"),
            Ok(Some(Action::Reset("openrouter".into(), None)))
        );
        assert_eq!(
            parse("priority openrouter work 0"),
            Ok(Some(Action::Priority(
                "openrouter".into(),
                "work".into(),
                0
            )))
        );
        assert_eq!(parse("q"), Ok(Some(Action::Quit)));
        assert!(parse("priority openrouter work first").is_err());
        assert!(parse("frobnicate").is_err());
    }
}

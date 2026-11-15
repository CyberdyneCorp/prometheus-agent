//! `/evolve [--k N]`: queues a job for the current session and answers at
//! once. The job's work never joins the turn, and the session's prompt is
//! untouched (an adopted harness applies from the next session).

use pmt_core::error::{Error, codes};
use pmt_evolve::job::{self, Request, TaskInput, Trigger};

use crate::chat::commands::Ctx;

/// Runs `/evolve`.
///
/// # Errors
///
/// `evolve.disabled`, `cli.usage` (a bad `--k`), `evolve.invalid_task`.
pub fn evolve(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    crate::surface::require("evolve")?;
    let scope = ctx.scope;
    pmt_evolve::gate::require_enabled(scope.config(), scope.name().as_str())?;
    let k = parse_k(args)?;
    let request = Request {
        k,
        ..Request::new(Trigger::Slash, TaskInput::Session(ctx.live.id))
    };
    let profile = super::profile(scope, Some(&ctx.factory.store));
    let record = job::queue(&profile, &request)?;
    Ok(super::queued_line(&record))
}

/// `--k N` (or nothing).
fn parse_k(args: &str) -> Result<Option<u8>, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] => Ok(None),
        ["--k", n] => n
            .parse::<u8>()
            .map(Some)
            .or_else(|_| job::budget::check_k(0).map(Some)),
        _ => Err(Error::new(&codes::CLI_USAGE, "usage: /evolve [--k N]")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k_is_the_only_argument() {
        assert_eq!(parse_k("").unwrap(), None);
        assert_eq!(parse_k("--k 3").unwrap(), Some(3));
        assert_eq!(
            parse_k("--k many").unwrap_err().message(),
            "--k must be between 1 and 8"
        );
        assert!(parse_k("now").is_err());
    }
}

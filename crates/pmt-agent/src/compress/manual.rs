//! `/compress` (spec: context-compression, Manual compaction command).
//!
//! Every surface parses the command with [`parse`] and runs it through
//! `Agent::compress`; [`render`] is the text a surface shows. A manual
//! run clears the summary-failure cooldown, the backoff and the
//! `ineffective` breaker before it starts.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};

use super::compactor::{Outcome, Status};

/// The command's usage line.
pub const USAGE: &str = "/compress [--preview] [here [N]] [<focus text>]";
/// Exchanges `here` keeps without a number.
pub const DEFAULT_KEEP: u32 = 2;

/// A parsed `/compress`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manual {
    /// `--preview`: report only.
    pub preview: bool,
    /// `here [N]`: keep exactly the last N user exchanges.
    pub keep_exchanges: Option<u32>,
    /// Passed to the summarizer as a priority instruction.
    pub focus: Option<String>,
}

/// Parses the arguments after `/compress`.
///
/// # Errors
///
/// `cli.usage` for `here 0`.
pub fn parse(args: &str) -> Result<Manual, Error> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let mut manual = Manual::default();
    if words.first() == Some(&"--preview") {
        manual.preview = true;
        words.remove(0);
    }
    if words.first() == Some(&"here") {
        words.remove(0);
        let n = match words.first().and_then(|w| w.parse::<u32>().ok()) {
            Some(n) => {
                words.remove(0);
                n
            }
            None => DEFAULT_KEEP,
        };
        if n == 0 {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "/compress here N keeps at least one exchange",
            )
            .with_hint(USAGE));
        }
        manual.keep_exchanges = Some(n);
    }
    let focus = words.join(" ");
    manual.focus = (!focus.is_empty()).then_some(focus);
    Ok(manual)
}

/// The text a surface shows for `outcome`.
#[must_use]
pub fn render(outcome: &Outcome) -> String {
    let tokens = format!(
        "{} -> {} tokens",
        outcome.tokens_before, outcome.tokens_after
    );
    match outcome.status {
        Status::Preview => format!(
            "preview: would summarize {} messages; about {tokens}",
            outcome.summarized.len()
        ),
        Status::NothingToDo => "nothing_to_do: nothing to compress yet".to_owned(),
        Status::LockSkipped => {
            "lock_skipped: another compaction of this session is running".to_owned()
        }
        Status::Compressed => {
            let mut text = format!(
                "compressed: {} messages summarized, {tokens} (epoch {})",
                outcome.summarized.len(),
                outcome.epoch.unwrap_or_default()
            );
            if let Some(failure) = &outcome.failure {
                let _ = write!(
                    text,
                    "; the summary failed ({}), a deterministic summary was used",
                    failure.message
                );
            }
            text
        }
        Status::Failed => format!(
            "failed: {}",
            outcome
                .failure
                .as_ref()
                .map_or("the transcript changed meanwhile", |f| f.message.as_str())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_parser_for_every_form() {
        let rows = [
            ("", Manual::default()),
            (
                "--preview",
                Manual {
                    preview: true,
                    ..Manual::default()
                },
            ),
            (
                "here",
                Manual {
                    keep_exchanges: Some(2),
                    ..Manual::default()
                },
            ),
            (
                "--preview here 5 the auth bug",
                Manual {
                    preview: true,
                    keep_exchanges: Some(5),
                    focus: Some("the auth bug".into()),
                },
            ),
            (
                "here keep the API design",
                Manual {
                    keep_exchanges: Some(2),
                    focus: Some("keep the API design".into()),
                    ..Manual::default()
                },
            ),
            (
                "  the   schema  ",
                Manual {
                    focus: Some("the schema".into()),
                    ..Manual::default()
                },
            ),
        ];
        for (args, expected) in rows {
            assert_eq!(parse(args).unwrap(), expected, "{args:?}");
        }
        assert_eq!(parse("here 0").unwrap_err().code().code, "cli.usage");
    }
}

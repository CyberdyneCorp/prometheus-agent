//! `/suggestions [list|accept <n> [--yes]|dismiss <n>|catalog|clear]`
//! (alias `/suggest`) on every chat surface (spec: scheduled-jobs,
//! Suggested automations inbox): the same inbox as `prometheus cron
//! suggestions`. Accepting goes through `/cron add` (the schedule echo is
//! shown, the job delivers to the invoking chat, and it is saved only
//! after `yes` or `--yes`); a blueprint suggestion asks for its missing
//! values first.

use pmt_core::error::{Error, codes};
use pmt_cron::suggest;

use super::Ctx;
use super::cron::{dispatch, words};
use crate::cron_host::blueprints;

fn usage() -> Error {
    Error::new(
        &codes::CLI_USAGE,
        "usage: /suggestions [list | accept <n> [--yes] | dismiss <n> | catalog | clear]",
    )
}

fn number(word: Option<&String>) -> Result<usize, Error> {
    word.and_then(|w| w.parse().ok()).ok_or_else(usage)
}

/// `/suggestions ...`.
///
/// # Errors
///
/// Usage errors, an unknown number, the errors of `/cron add`.
pub(super) fn suggestions(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let _ = super::cron::take(ctx.live.id, "");
    let words = words(args)?;
    let store = &ctx.factory.store;
    match words.first().map(String::as_str) {
        None | Some("list") => {
            blueprints::offer_skill_blueprints(store, ctx.scope, ctx.factory.launch())?;
            Ok(suggest::render(&*ctx.strings(), &suggest::pending(store)?))
        }
        Some("accept") => {
            let yes = words.iter().any(|w| w == "--yes");
            accept(ctx, number(words.get(1))?, yes)
        }
        Some("dismiss") => {
            let row = suggest::dismiss(store, number(words.get(1))?)?;
            Ok(pmt_i18n::t!(
                ctx.strings(),
                "slash.suggestions.dismissed",
                title = row.title
            ))
        }
        Some("catalog") => {
            let added = suggest::offer_catalog(store)?;
            let head = pmt_i18n::t!(ctx.strings(), "slash.suggestions.added", count = added);
            Ok(format!(
                "{head}\n{}",
                suggest::render(&*ctx.strings(), &suggest::pending(store)?)
            ))
        }
        Some("clear") => {
            let dropped = store.cron_clear_suggestions()?;
            Ok(pmt_i18n::t!(
                ctx.strings(),
                "slash.suggestions.cleared",
                count = dropped
            ))
        }
        Some(_) => Err(usage()),
    }
}

/// Suggestion `n` as `/cron add`, or its blueprint's questions.
fn accept(ctx: &Ctx<'_>, n: usize, yes: bool) -> Result<String, Error> {
    let row = suggest::numbered(&ctx.factory.store, n)?;
    if row.schedule.is_empty() {
        return super::blueprint::start(ctx, &row.key, &row.key, yes);
    }
    let mut words = vec![
        "add".to_owned(),
        row.schedule,
        row.prompt,
        "--name".to_owned(),
        row.title,
        "--from-suggestion".to_owned(),
        row.key,
    ];
    if yes {
        words.push("--yes".to_owned());
    }
    dispatch(ctx, words)
}

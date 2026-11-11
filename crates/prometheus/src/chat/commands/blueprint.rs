//! `/blueprint [name] [slot=value ...] [--yes]` (alias `/bp`) on every chat
//! surface (spec: scheduled-jobs, Automation blueprint catalog): without a
//! name it lists the catalog; a name resolves by exact key, prefix or
//! nearest spelling; each missing required slot is asked one question at
//! a time (the session's next input answers it); the filled blueprint
//! becomes `/cron add <schedule> <prompt> --name <title>`, so the schedule
//! echo is shown and the job (delivering to the invoking chat) is saved
//! only after the `yes` reply or `--yes`. An invalid value given inline is
//! refused and creates nothing; an invalid answer is asked again.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_cron::blueprint::{self, Blueprint, Fill};

use super::Ctx;
use super::cron::{Pending, dispatch, wait_for, words};
use crate::cron_host::blueprints;

/// A blueprint waiting for the answer to one of its questions.
#[derive(Debug, Clone)]
pub struct Asking {
    /// The blueprint's key.
    key: String,
    /// The values given so far.
    values: BTreeMap<String, String>,
    /// The slot asked for.
    slot: String,
    /// The suggestion being accepted, if any.
    suggestion: Option<String>,
}

/// The profile's catalog.
fn catalog(ctx: &Ctx<'_>) -> Vec<Blueprint> {
    blueprints::catalog(ctx.scope, ctx.factory.launch())
}

/// The catalog, one line per blueprint.
fn listing(strings: &pmt_i18n::Strings, catalog: &[Blueprint]) -> String {
    let mut text = pmt_i18n::t!(strings, "slash.blueprint.heading");
    for b in catalog {
        let slots: Vec<&str> = b.slots.iter().map(|s| s.name.as_str()).collect();
        let slots = if slots.is_empty() {
            "-".to_owned()
        } else {
            slots.join(", ")
        };
        let line = pmt_i18n::t!(
            strings,
            "slash.blueprint.line",
            key = b.key,
            title = b.title,
            category = b.category,
            slots = slots
        );
        let _ = write!(text, "\n{line}");
    }
    text
}

/// `/blueprint ...`.
///
/// # Errors
///
/// `cron.blueprint` (unknown name, invalid or unknown slot), the errors of
/// `/cron add`.
pub(super) fn blueprint(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let _ = super::cron::take(ctx.live.id, "");
    let mut words = words(args)?;
    let yes = words.iter().any(|w| w == "--yes");
    words.retain(|w| w != "--yes");
    let catalog = catalog(ctx);
    let Some((name, rest)) = words.split_first() else {
        return Ok(listing(&ctx.strings(), &catalog));
    };
    let chosen = blueprint::resolve(&catalog, name)?;
    let values = blueprint::values(rest)?;
    proceed(ctx, chosen, values, (None, yes))
}

/// Starts filling `key` for suggestion `suggestion` (accepting a
/// blueprint suggestion).
///
/// # Errors
///
/// As [`blueprint`].
pub(super) fn start(
    ctx: &Ctx<'_>,
    key: &str,
    suggestion: &str,
    yes: bool,
) -> Result<String, Error> {
    let catalog = catalog(ctx);
    let chosen = catalog.iter().find(|b| b.key == key).ok_or_else(|| {
        Error::new(
            &codes::CRON_BLUEPRINT,
            format!("blueprint '{key}' is no longer installed"),
        )
    })?;
    proceed(
        ctx,
        chosen,
        BTreeMap::new(),
        (Some(suggestion.to_owned()), yes),
    )
}

/// Asks the next missing slot, or shows the echo of the filled job.
fn proceed(
    ctx: &Ctx<'_>,
    chosen: &Blueprint,
    values: BTreeMap<String, String>,
    (suggestion, yes): (Option<String>, bool),
) -> Result<String, Error> {
    match chosen.fill(&values)? {
        Fill::Missing(slot) => {
            let question = format!("{}: {}", chosen.title, slot.prompt());
            wait_for(
                ctx.live.id,
                Pending::Question(Asking {
                    key: chosen.key.clone(),
                    values,
                    slot: slot.name,
                    suggestion,
                }),
            );
            Ok(question)
        }
        Fill::Ready(filled) => {
            // What the job will do, above the schedule echo.
            let job = format!("{}: {}", filled.title, filled.prompt);
            let mut words = vec![
                "add".to_owned(),
                filled.schedule,
                filled.prompt,
                "--name".to_owned(),
                filled.title,
            ];
            if let Some(key) = suggestion {
                words.extend(["--from-suggestion".to_owned(), key]);
            }
            if yes {
                words.push("--yes".to_owned());
            }
            Ok(format!("{job}\n{}", dispatch(ctx, words)?))
        }
    }
}

/// The answer `text` to the question `asking` asked.
///
/// # Errors
///
/// As [`blueprint`].
pub(super) fn answer(ctx: &Ctx<'_>, asking: Asking, text: &str) -> Result<String, Error> {
    let catalog = catalog(ctx);
    let Some(chosen) = catalog.iter().find(|b| b.key == asking.key) else {
        return Err(Error::new(
            &codes::CRON_BLUEPRINT,
            format!("blueprint '{}' is no longer installed", asking.key),
        ));
    };
    let mut values = asking.values.clone();
    values.insert(asking.slot.clone(), text.to_owned());
    match chosen.fill(&values) {
        Err(err) if err.code().code == codes::CRON_BLUEPRINT.code => {
            // Asked again until the answer fits.
            let again = format!("{}\n{}", err.message(), question_of(chosen, &asking.slot));
            wait_for(ctx.live.id, Pending::Question(asking));
            Ok(again)
        }
        Err(err) => Err(err),
        Ok(_) => proceed(ctx, chosen, values, (asking.suggestion, false)),
    }
}

fn question_of(chosen: &Blueprint, slot: &str) -> String {
    chosen
        .slots
        .iter()
        .find(|s| s.name == slot)
        .map(|s| format!("{}: {}", chosen.title, s.prompt()))
        .unwrap_or_default()
}

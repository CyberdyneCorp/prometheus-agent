//! `prometheus cron suggestions [list|accept <n> [--yes]|dismiss <n>|
//! catalog|clear]` (spec: scheduled-jobs, Suggested automations inbox):
//! the same inbox as `/suggestions`. Listing first offers the installed
//! skills' blueprints; accepting goes through `cron add` (echo, then
//! confirmation) and marks the suggestion accepted once the job is saved.

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_cron::suggest;
use pmt_store::cron::suggestions::SuggestionRow;
use serde_json::json;

use super::edit::{Creation, add};
use super::{Env, JobFlags, SuggestionAction};
use crate::cron_host::blueprints;
use crate::output::{document, print_raw};

pub(super) fn run(
    env: &Env<'_>,
    action: Option<&SuggestionAction>,
    now: Timestamp,
) -> Result<u8, Error> {
    match action.unwrap_or(&SuggestionAction::List) {
        SuggestionAction::List => list(env),
        SuggestionAction::Accept { n, yes } => accept(env, *n, *yes, now),
        SuggestionAction::Dismiss { n } => {
            let row = suggest::dismiss(&env.store, *n)?;
            print_raw(&format!(
                "dismissed {} ({}); it is not offered again\n",
                row.title, row.key
            ))
        }
        SuggestionAction::Catalog => {
            let added = suggest::offer_catalog(&env.store)?;
            print_raw(&format!("added {added} starter suggestion(s)\n"))
        }
        SuggestionAction::Clear => {
            let dropped = env.store.cron_clear_suggestions()?;
            print_raw(&format!("cleared {dropped} accepted suggestion(s)\n"))
        }
    }
}

fn row_json(n: usize, row: &SuggestionRow) -> serde_json::Value {
    json!({"n": n, "key": row.key, "source": row.source, "title": row.title,
           "schedule": row.schedule, "prompt": row.prompt})
}

fn list(env: &Env<'_>) -> Result<u8, Error> {
    blueprints::offer_skill_blueprints(&env.store, &env.scope, env.ctx.launch)?;
    let rows = suggest::pending(&env.store)?;
    if env.ctx.json {
        let items: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| row_json(i + 1, r))
            .collect();
        return document("cron suggestions", &json!({"suggestions": items}));
    }
    let english = pmt_i18n::Strings::english(pmt_i18n::Surface::Core);
    print_raw(&format!("{}\n", suggest::render(&*english, &rows)))
}

/// `cron add` of suggestion `n`.
fn accept(env: &Env<'_>, n: usize, yes: bool, now: Timestamp) -> Result<u8, Error> {
    let row = suggest::numbered(&env.store, n)?;
    if row.schedule.is_empty() {
        return Err(Error::new(
            &codes::CRON_BLUEPRINT,
            format!(
                "suggestion {n} ({}) needs values: fill it in a chat with `/suggestions accept {n}` \
                 or `/blueprint {} slot=value ...`",
                row.title, row.key
            ),
        ));
    }
    let flags = JobFlags {
        prompt: Some(row.prompt.clone()),
        name: Some(row.title.clone()),
        yes,
        ..JobFlags::default()
    };
    let how = Creation {
        paused: None,
        suggestion: Some(&row.key),
    };
    add(env, Some(&row.schedule), &flags, how, now)
}

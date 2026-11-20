//! Natural-language schedules (spec: scheduled-jobs, Natural-language
//! schedule translation): `prometheus cron add|edit --nl "<text>"` and the
//! `cronjob` tool's `schedule_text` are translated by the profile's
//! `schedule` auxiliary route; the answer is accepted only when it parses
//! with the deterministic grammar or as a supported RFC 5545 rule
//! ([`pmt_cron::schedule::translate::accept`]), and the caller echoes it
//! for confirmation as for any schedule. A text the grammar already
//! accepts needs no model.

use std::path::PathBuf;
use std::sync::Arc;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_cron::fields::Fields;
use pmt_cron::schedule::Schedule;
use pmt_cron::schedule::translate::{INSTRUCTIONS, accept, prompt, refused, remember, remembered};
use pmt_providers::auxiliary::AuxTask;
use pmt_store::Store;
use pmt_store::model::Source;
use pmt_store::sessions::NewSession;

use crate::chat::session::model::{Models, RouteFlags};

/// The longest answer a translation needs.
const MAX_OUTPUT_TOKENS: u64 = 200;

/// The schedule `text` names in `zone`, its usage recorded under
/// `session`.
///
/// # Errors
///
/// `cron.invalid_schedule` (`could not translate schedule: <text>`); the
/// auxiliary route's resolution errors.
pub(crate) async fn translate(
    models: &Models,
    session: SessionId,
    text: &str,
    zone: &TimeZone,
    now: Timestamp,
) -> Result<Schedule, Error> {
    if let Some(kept) = remembered(text, zone, now) {
        return Ok(kept);
    }
    let schedule = if let Ok(schedule) = Schedule::parse(text, now, zone) {
        schedule
    } else {
        let reply = models
            .ask_auxiliary(
                AuxTask::Schedule,
                session,
                INSTRUCTIONS,
                &prompt(text, now, zone),
                MAX_OUTPUT_TOKENS,
            )
            .await?
            .ok_or_else(|| refused(text))?;
        accept(&reply, text, now, zone)?
    };
    remember(text, zone, &schedule, now);
    Ok(schedule)
}

/// The zone a job of `fields` runs in: its pinned `timezone`, else
/// `fallback` (the job's or the profile's).
///
/// # Errors
///
/// `cron.invalid_job` for an unknown zone.
pub(crate) fn zone_of(fields: &Fields, fallback: &TimeZone) -> Result<TimeZone, Error> {
    match fields.timezone.as_deref().filter(|z| !z.is_empty()) {
        Some(name) => TimeZone::get(name).map_err(|e| {
            Error::new(
                &pmt_core::error::codes::CRON_INVALID_JOB,
                format!("unknown time zone '{name}': {e}"),
            )
        }),
        None => Ok(fallback.clone()),
    }
}

/// Translates `fields.schedule_text` for a command outside any session:
/// the model call is accounted to a hidden session of its own.
///
/// # Errors
///
/// As [`translate`]; store errors.
pub(crate) fn fill(
    scope: &ProfileScope,
    store: &Arc<Store>,
    launch: &LaunchEnv,
    fields: &mut Fields,
    zone: &TimeZone,
) -> Result<(), Error> {
    let Some(text) = fields.schedule_text.clone() else {
        return Ok(());
    };
    let user_home: Option<PathBuf> = launch.user_home().cloned();
    let models = Models::open(
        scope,
        store,
        user_home.as_deref(),
        &RouteFlags::default(),
        None,
    )?;
    let session = store.create_session(&NewSession {
        source_tag: Some("cron-schedule".to_owned()),
        hidden: true,
        ..NewSession::new(Source::Cli)
    })?;
    let now = Timestamp::now();
    fields.translated = Some(crate::model_cmd::block_on(translate(
        &models, session.id, &text, zone, now,
    ))??);
    Ok(())
}

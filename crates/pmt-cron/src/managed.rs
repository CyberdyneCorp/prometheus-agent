//! The managed scheduler contract (spec: scheduled-jobs, Managed
//! scheduler contract): with `cron.provider = "managed"` an external
//! scheduler signals each occurrence. Every enabled job is armed there as
//! one one-shot at its `next_run_at` under `dedup_key =
//! "<job_id>:<fire_at>"` ([`arms`]); its fire comes back as a request
//! carrying a JWT whose claims [`claims_ok`] checks (the binary verifies
//! the signature against the scheduler's JWKS, asymmetric algorithms
//! only), and becomes a run through the same at-most-once claim as the
//! built-in trigger ([`claim_fire`]): a fire repeated for an occurrence
//! already claimed finds the job advanced and starts nothing.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::schema::CronConfig;
use pmt_core::error::Error;
use pmt_store::Store;
use pmt_store::cron::RunRow;
use serde::Serialize;
use serde_json::Value;

use crate::job::{Job, JobState};
use crate::schedule::at_ms;

/// The `purpose` claim of a fire token.
pub const PURPOSE: &str = "cron_fire";

/// Clock skew tolerated on `exp` and `nbf`.
pub const LEEWAY_SECS: i64 = 30;

/// `<job_id>:<fire_at>` with the instant in RFC 3339 UTC (milliseconds
/// shown when it has them, so the key names the exact instant).
#[must_use]
pub fn dedup_key(job_id: &str, fire_at_ms: i64) -> String {
    format!("{job_id}:{}", at_ms(fire_at_ms))
}

/// One external one-shot to arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Arm {
    /// The job.
    pub job_id: String,
    /// When it fires (Unix milliseconds).
    pub fire_at_ms: i64,
    /// What the scheduler deduplicates on.
    pub dedup_key: String,
}

/// The one-shots the profile's jobs need now: one per enabled job (not in
/// `error`) with a next run.
///
/// # Errors
///
/// Store errors.
pub fn arms(store: &Store) -> Result<Vec<Arm>, Error> {
    Ok(store
        .cron_jobs()?
        .into_iter()
        .filter(|row| row.enabled && row.state != JobState::Error.as_str())
        .filter_map(|row| {
            let at = row.next_run_at?;
            Some(Arm {
                dedup_key: dedup_key(&row.id, at),
                job_id: row.id,
                fire_at_ms: at,
            })
        })
        .collect())
}

/// Whether the verified `claims` of a fire token are acceptable at `now`:
/// `aud` names `audience`, `purpose` is `cron_fire`, `exp` (required) and
/// `nbf` hold within the leeway, and a `dedup_key` claim, when present,
/// is the fire's.
#[must_use]
pub fn claims_ok(claims: &Value, audience: &str, dedup_key: &str, now: Timestamp) -> bool {
    let now = now.as_second();
    let audience_ok = match &claims["aud"] {
        Value::String(aud) => aud == audience,
        Value::Array(auds) => auds.iter().any(|a| a.as_str() == Some(audience)),
        _ => false,
    };
    let exp_ok = claims["exp"]
        .as_i64()
        .is_some_and(|exp| now <= exp + LEEWAY_SECS);
    let nbf_ok = match &claims["nbf"] {
        Value::Null => true,
        nbf => nbf.as_i64().is_some_and(|nbf| now + LEEWAY_SECS >= nbf),
    };
    let key_ok = claims["dedup_key"].as_str().is_none_or(|k| k == dedup_key);
    audience_ok && claims["purpose"] == PURPOSE && exp_ok && nbf_ok && key_ok
}

/// What a verified fire did.
#[derive(Debug, Clone, PartialEq)]
pub enum Fired {
    /// It claimed this run, which must now execute.
    Run(Box<(Job, RunRow)>),
    /// The occurrence was already claimed, the job moved on, or it is
    /// paused: nothing to do (answered 200 all the same).
    Nothing,
    /// The profile has no such job.
    Unknown,
}

/// Claims the occurrence of `job_id` at `fire_at_ms` that a managed fire
/// signals. Only the job's armed occurrence (its current `next_run_at`)
/// fires; the managed scheduler decides when it is signalled, so an
/// early fire is claimed as at its instant.
///
/// # Errors
///
/// Store errors.
pub fn claim_fire(
    store: &Store,
    (job_id, fire_at_ms): (&str, i64),
    now: Timestamp,
    profile_zone: &TimeZone,
    config: &CronConfig,
) -> Result<Fired, Error> {
    let Some(row) = store.cron_job(job_id)? else {
        return Ok(Fired::Unknown);
    };
    let armed =
        row.enabled && row.state != JobState::Error.as_str() && row.next_run_at == Some(fire_at_ms);
    if !armed {
        return Ok(Fired::Nothing);
    }
    let at = now.max(at_ms(fire_at_ms));
    let job = Job::from_row(row)?;
    Ok(
        match crate::scheduler::claim_due(store, job, at, profile_zone, config)? {
            Some(run) => Fired::Run(Box::new(run)),
            None => Fired::Nothing,
        },
    )
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use serde_json::json;

    use super::{claims_ok, dedup_key};

    #[test]
    fn dedup_keys_name_the_job_and_the_utc_instant() {
        assert_eq!(
            dedup_key("a1b2c3d4e5f6", 1_795_000_000_000),
            "a1b2c3d4e5f6:2026-11-18T11:06:40Z"
        );
        assert_eq!(
            dedup_key("a1b2c3d4e5f6", 1_795_000_000_250),
            "a1b2c3d4e5f6:2026-11-18T11:06:40.25Z"
        );
    }

    #[test]
    fn fire_token_claims_are_checked_within_the_leeway() {
        let now: Timestamp = "2026-11-17T10:00:00Z".parse().unwrap();
        let t = now.as_second();
        let key = "a1b2c3d4e5f6:2026-11-17T10:00:00Z";
        let good = json!({"aud": "prometheus", "purpose": "cron_fire", "exp": t + 60, "nbf": t});
        assert!(claims_ok(&good, "prometheus", key, now));
        let with = |patch: serde_json::Value| {
            let mut claims = good.clone();
            for (k, v) in patch.as_object().unwrap() {
                claims[k] = v.clone();
            }
            claims_ok(&claims, "prometheus", key, now)
        };
        assert!(with(json!({"aud": ["other", "prometheus"]})));
        assert!(with(json!({"exp": t - 29})), "within the leeway");
        assert!(!with(json!({"exp": t - 31})));
        assert!(!with(json!({"exp": null})));
        assert!(with(json!({"nbf": t + 29})));
        assert!(!with(json!({"nbf": t + 31})));
        assert!(!with(json!({"aud": "someone-else"})));
        assert!(!with(json!({"purpose": "login"})));
        assert!(with(json!({"dedup_key": key})));
        assert!(!with(json!({"dedup_key": "other:2026-11-17T10:00:00Z"})));
    }
}

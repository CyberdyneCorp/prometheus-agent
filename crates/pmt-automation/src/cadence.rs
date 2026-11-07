//! Loop and heartbeat timing (spec: session-automation, Loop command and
//! cadence; Loop stop conditions; Session heartbeat).
//!
//! A wakeup is reserved when it is admitted ([`fire_loop`],
//! [`fire_heartbeat`]) and the turn's end then updates the loop
//! ([`after_loop_turn`]). Hosts keep the state from before the
//! reservation and put it back when the admission is refused before the
//! turn starts, so a refused attempt costs no tick.

use std::fmt::Write as _;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::command::LoopSpec;
use crate::state::{Cadence, Heartbeat, Limits, Loop, RunStatus};

/// The line a loop's reply ends with to end the loop.
pub const LOOP_COMPLETE: &str = "LOOP_COMPLETE";

/// Final replies of a heartbeat turn that deliver nothing.
pub const SILENT_REPLIES: [&str; 2] = ["NO_REPLY", "[SILENT]"];

/// What every heartbeat fire is prefixed with.
pub const HEARTBEAT_PREFIX: &str = "[heartbeat] Reply briefly. If nothing meaningful changed since the last check, reply exactly NO_REPLY and stop.";

/// Whether `reply` is a heartbeat's "nothing to say".
#[must_use]
pub fn is_silent(reply: &str) -> bool {
    SILENT_REPLIES.contains(&reply.trim())
}

/// The heartbeat's injected prompt.
#[must_use]
pub fn heartbeat_prompt(prompt: &str) -> String {
    format!("{HEARTBEAT_PREFIX}\n\n{prompt}")
}

/// The loop's injected prompt (a slash command runs as the command).
#[must_use]
pub fn loop_prompt(looping: &Loop) -> String {
    if looping.prompt.starts_with('/') {
        return looping.prompt.clone();
    }
    format!(
        "[loop wakeup {}] {}\n\n(End your reply with a line {LOOP_COMPLETE} when there is nothing left to watch.)",
        looping.fired, looping.prompt
    )
}

fn ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

/// A new loop: its first wakeup is due at the next idle boundary.
#[must_use]
pub fn new_loop(spec: LoopSpec, now_ms: i64, limits: &Limits) -> Loop {
    let cadence = match spec.every_secs {
        Some(every_secs) => Cadence::Fixed { every_secs },
        None => Cadence::SelfPaced {
            wait_secs: limits.self_paced_floor_secs,
            digest: None,
        },
    };
    Loop {
        prompt: spec.prompt,
        cadence,
        times: spec.times,
        until: spec.until,
        fired: 0,
        status: RunStatus::Active,
        next_fire_ms: now_ms,
        reason: None,
        route: None,
    }
}

/// Reserves a loop wakeup admitted at `now_ms`.
pub fn fire_loop(looping: &mut Loop, now_ms: i64) {
    looping.fired += 1;
    if let Cadence::Fixed { every_secs } = looping.cadence {
        looping.next_fire_ms = now_ms + ms(every_secs);
    }
}

/// What a loop wakeup turn's end does to the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEnd {
    /// It keeps going (ask the judge first when it has `--until`).
    Next,
    /// It ended: remove it.
    Removed(String),
    /// It paused.
    Paused(String),
}

/// A loop wakeup turn ended at `now_ms` with `reply` (`interrupted`: the
/// user stopped it).
pub fn after_loop_turn(
    looping: &mut Loop,
    reply: &str,
    interrupted: bool,
    now_ms: i64,
    limits: &Limits,
) -> LoopEnd {
    if interrupted {
        return pause(looping, "the wakeup turn was interrupted".to_owned());
    }
    if reply.trim_end().lines().last().map(str::trim) == Some(LOOP_COMPLETE) {
        return LoopEnd::Removed("Loop complete.".to_owned());
    }
    if let Some(end) = exhausted(looping, limits) {
        return end;
    }
    if let Cadence::SelfPaced { wait_secs, digest } = &mut looping.cadence {
        let current = reply_digest(reply);
        let unchanged = digest.as_ref().is_none_or(|d| *d == current);
        *wait_secs = if unchanged {
            wait_secs
                .saturating_mul(2)
                .min(limits.self_paced_ceiling_secs)
        } else {
            limits.self_paced_floor_secs
        };
        *digest = Some(current);
        looping.next_fire_ms = now_ms + ms(*wait_secs);
    }
    LoopEnd::Next
}

/// The end of a loop whose wakeups are spent: `--times N` removes it,
/// `loops.max_ticks` pauses it. Also checked before a wakeup fires, so a
/// loop whose wakeup ran a slash command (no turn to end) stops too.
pub fn exhausted(looping: &mut Loop, limits: &Limits) -> Option<LoopEnd> {
    if looping.times.is_some_and(|times| looping.fired >= times) {
        return Some(LoopEnd::Removed(format!(
            "Loop finished after {} runs.",
            looping.fired
        )));
    }
    if limits.loop_max_ticks > 0 && looping.fired >= limits.loop_max_ticks {
        let reason = format!(
            "{} wakeups (loops.max_ticks); /loop resume to keep going",
            looping.fired
        );
        return Some(pause(looping, reason));
    }
    None
}

/// Applies the `--until` judge verdict (`Err`: the judge failed, which
/// keeps the loop going).
pub fn until(looping: &mut Loop, verdict: Result<crate::judge::Verdict, String>) -> LoopEnd {
    use crate::judge::Verdict;
    match verdict {
        Ok(Verdict::Done(reason)) => LoopEnd::Removed(with_reason("Loop condition met", &reason)),
        Ok(Verdict::Blocked(reason)) => pause(looping, format!("judged unachievable: {reason}")),
        Ok(_) | Err(_) => LoopEnd::Next,
    }
}

fn with_reason(head: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!("{head}.")
    } else {
        format!("{head}: {reason}")
    }
}

fn pause(looping: &mut Loop, reason: String) -> LoopEnd {
    looping.status = RunStatus::Paused;
    let notice = format!("Loop paused: {reason}");
    looping.reason = Some(reason);
    LoopEnd::Paused(notice)
}

/// Resumes a paused loop: its next wakeup is due at the next idle
/// boundary and `loops.max_ticks` counts from here.
pub fn resume_loop(looping: &mut Loop, now_ms: i64) {
    looping.status = RunStatus::Active;
    looping.reason = None;
    looping.next_fire_ms = now_ms;
    if looping.times.is_none() {
        looping.fired = 0;
    }
}

/// The digest of a reply with its timestamps ignored.
#[must_use]
pub fn reply_digest(reply: &str) -> String {
    static STAMPS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"\d{4}-\d{2}-\d{2}([T ]\d{1,2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:?\d{2})?)?|\b\d{1,2}:\d{2}(:\d{2})?\b",
        )
        .expect("a valid pattern")
    });
    let stripped = STAMPS.replace_all(reply.trim(), "<t>");
    let hash = Sha256::digest(stripped.as_bytes());
    hash.iter()
        .take(16)
        .fold(String::with_capacity(32), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// A new heartbeat: its first fire is one interval away.
#[must_use]
pub fn new_heartbeat(every_secs: u64, prompt: String, now_ms: i64) -> Heartbeat {
    Heartbeat {
        prompt,
        every_secs,
        status: RunStatus::Active,
        next_fire_ms: now_ms + ms(every_secs),
        fired: 0,
        route: None,
    }
}

/// Reserves a heartbeat fire at `now_ms`: however many intervals were
/// missed, this is one fire, and the timer re-anchors on it.
pub fn fire_heartbeat(heartbeat: &mut Heartbeat, now_ms: i64) {
    heartbeat.fired += 1;
    heartbeat.next_fire_ms = now_ms + ms(heartbeat.every_secs);
}

/// Resumes a heartbeat: it re-anchors, never firing at once.
pub fn resume_heartbeat(heartbeat: &mut Heartbeat, now_ms: i64) {
    heartbeat.status = RunStatus::Active;
    heartbeat.next_fire_ms = now_ms + ms(heartbeat.every_secs);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_paced() -> Loop {
        new_loop(
            LoopSpec {
                every_secs: None,
                prompt: "keep an eye on the migration".into(),
                times: None,
                until: None,
            },
            0,
            &Limits::default(),
        )
    }

    fn wait(looping: &Loop) -> u64 {
        match looping.cadence {
            Cadence::SelfPaced { wait_secs, .. } => wait_secs,
            Cadence::Fixed { every_secs } => every_secs,
        }
    }

    #[test]
    fn self_pacing_backs_off_on_identical_replies_and_snaps_back() {
        let limits = Limits::default();
        let mut looping = self_paced();
        for at in ["at 10:01:00", "at 10:02:30", "at 2026-11-07T10:05:00Z"] {
            fire_loop(&mut looping, 0);
            after_loop_turn(
                &mut looping,
                &format!("migration 40% {at}"),
                false,
                0,
                &limits,
            );
        }
        assert_eq!(wait(&looping), 480);
        assert_eq!(looping.next_fire_ms, 480_000);
        fire_loop(&mut looping, 0);
        after_loop_turn(&mut looping, "migration 80%", false, 0, &limits);
        assert_eq!(wait(&looping), 60);
    }

    #[test]
    fn self_pacing_stops_at_the_ceiling() {
        let limits = Limits::default();
        let mut looping = self_paced();
        for _ in 0..10 {
            after_loop_turn(&mut looping, "same", false, 0, &limits);
        }
        assert_eq!(wait(&looping), 900);
    }

    #[test]
    fn the_run_cap_and_loop_complete_remove_the_loop() {
        let limits = Limits::default();
        let mut looping = new_loop(
            LoopSpec {
                every_secs: Some(120),
                prompt: "poll CI".into(),
                times: Some(3),
                until: None,
            },
            0,
            &limits,
        );
        for _ in 0..2 {
            fire_loop(&mut looping, 0);
            assert_eq!(
                after_loop_turn(&mut looping, "pending", false, 0, &limits),
                LoopEnd::Next
            );
        }
        fire_loop(&mut looping, 0);
        assert!(matches!(
            after_loop_turn(&mut looping, "pending", false, 0, &limits),
            LoopEnd::Removed(_)
        ));
        let mut other = self_paced();
        assert!(matches!(
            after_loop_turn(&mut other, "all good\nLOOP_COMPLETE\n", false, 0, &limits),
            LoopEnd::Removed(_)
        ));
    }

    #[test]
    fn an_interrupted_wakeup_and_max_ticks_pause() {
        let limits = Limits {
            loop_max_ticks: 2,
            ..Limits::default()
        };
        let mut looping = self_paced();
        assert!(matches!(
            after_loop_turn(&mut looping, "", true, 0, &limits),
            LoopEnd::Paused(_)
        ));
        assert_eq!(looping.status, RunStatus::Paused);
        resume_loop(&mut looping, 5);
        fire_loop(&mut looping, 5);
        fire_loop(&mut looping, 5);
        assert!(matches!(
            after_loop_turn(&mut looping, "x", false, 5, &limits),
            LoopEnd::Paused(_)
        ));
    }

    /// `--until`: yes stops the loop, unachievable pauses it with the
    /// reason, and a failed or undecided judge keeps it going.
    #[test]
    fn the_until_verdict_stops_pauses_or_keeps_the_loop() {
        use crate::judge::Verdict;
        let mut looping = self_paced();
        assert_eq!(
            until(
                &mut looping,
                Ok(Verdict::Done("the deploy is green".into()))
            ),
            LoopEnd::Removed("Loop condition met: the deploy is green".into())
        );
        assert_eq!(
            until(&mut looping, Ok(Verdict::Continue("still red".into()))),
            LoopEnd::Next
        );
        assert_eq!(
            until(&mut looping, Err("provider unavailable".into())),
            LoopEnd::Next
        );
        assert_eq!(looping.status, RunStatus::Active);
        assert_eq!(
            until(
                &mut looping,
                Ok(Verdict::Blocked("the branch was deleted".into()))
            ),
            LoopEnd::Paused("Loop paused: judged unachievable: the branch was deleted".into())
        );
        assert_eq!(looping.status, RunStatus::Paused);
        assert_eq!(
            looping.reason.as_deref(),
            Some("judged unachievable: the branch was deleted")
        );
    }

    #[test]
    fn missed_heartbeats_coalesce_into_one_fire_and_re_anchor() {
        let mut heartbeat = new_heartbeat(600, "check CI".into(), 0);
        assert_eq!(heartbeat.next_fire_ms, 600_000);
        let idle_at = 35 * 60 * 1_000;
        assert!(heartbeat.next_fire_ms <= idle_at);
        fire_heartbeat(&mut heartbeat, idle_at);
        assert_eq!(heartbeat.fired, 1);
        assert_eq!(heartbeat.next_fire_ms, idle_at + 600_000);
        resume_heartbeat(&mut heartbeat, idle_at + 1);
        assert_eq!(heartbeat.next_fire_ms, idle_at + 1 + 600_000);
        assert!(is_silent(" NO_REPLY\n") && is_silent("[SILENT]") && !is_silent("ok"));
    }
}

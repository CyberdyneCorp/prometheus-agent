//! The editable fields of a job, as the `cron` CLI flags and the
//! `cronjob` tool arguments give them, applied to a definition.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};

use crate::duration;
use crate::job::{JobSpec, MemoryMode, SELF};
use crate::schedule::Schedule;

/// A change of the model pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// Pin this model (and provider): `--model`, or `--pin` / tool
    /// `pinned: true` with the profile's current main model.
    Set {
        /// The model.
        model: String,
        /// The provider.
        provider: Option<String>,
    },
    /// Follow the cron default again (`--unpin`, tool `pinned: false`).
    Release,
}

/// Fields to set; `None` leaves a field as it is. For text fields an
/// empty string clears the field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    /// The schedule, in the grammar.
    pub schedule: Option<String>,
    /// A free-form schedule for the auxiliary model.
    pub schedule_text: Option<String>,
    /// The prompt.
    pub prompt: Option<String>,
    /// Preloaded skills.
    pub skills: Option<Vec<String>>,
    /// The script.
    pub script: Option<String>,
    /// Script-only job.
    pub no_agent: Option<bool>,
    /// Delivery targets.
    pub deliver: Option<Vec<String>>,
    /// Failure notice targets (`[""]` clears).
    pub failure_deliver: Option<Vec<String>>,
    /// Continuable deliveries.
    pub attach_to_session: Option<bool>,
    /// Toolsets.
    pub toolsets: Option<Vec<String>>,
    /// `read_only`, `read_write` or `off`.
    pub memory: Option<String>,
    /// Grace (`0s` to `24h`).
    pub grace: Option<String>,
    /// Idle timeout.
    pub idle: Option<String>,
    /// Wall timeout.
    pub wall: Option<String>,
    /// Runs before completion.
    pub repeat: Option<u32>,
    /// The model pin.
    pub pin: Option<Pin>,
    /// Pinned reasoning level.
    pub reasoning_effort: Option<String>,
    /// Pinned IANA zone.
    pub timezone: Option<String>,
    /// The run's working directory.
    pub workdir: Option<String>,
    /// Secrets passed to the script.
    pub script_secrets: Option<Vec<String>>,
    /// The `.py` interpreter.
    pub interpreter: Option<String>,
    /// The monitor script.
    pub monitor_script: Option<String>,
    /// The monitored URL.
    pub monitor_url: Option<String>,
    /// Upstream jobs (`self` kept as given by `continuity`).
    pub context_from: Option<Vec<String>>,
    /// Toggles the `self` entry of `context_from`.
    pub continuity: Option<bool>,
}

fn invalid(message: String) -> Error {
    Error::new(&codes::CRON_INVALID_JOB, message)
}

fn limit(name: &str, text: &str) -> Result<u64, Error> {
    duration::parse_limit(text).ok_or_else(|| {
        invalid(format!(
            "{name} '{text}' is not a duration such as 30s, 10m or 2h"
        ))
    })
}

/// Sets an optional text field: an empty string clears it.
fn set(field: &mut Option<String>, value: Option<&String>) {
    if let Some(value) = value {
        *field = Some(value.clone()).filter(|v| !v.is_empty());
    }
}

/// A list where a single empty entry (or none) clears the field.
fn list(value: &[String]) -> Vec<String> {
    value.iter().filter(|v| !v.is_empty()).cloned().collect()
}

impl Fields {
    /// A new job's definition (the schedule is required).
    ///
    /// # Errors
    ///
    /// `cron.invalid_schedule`, `cron.invalid_job`.
    pub fn new_spec(&self, now: Timestamp, profile_zone: &TimeZone) -> Result<JobSpec, Error> {
        if self.schedule.is_none() && self.schedule_text.is_none() {
            return Err(invalid("a job needs a schedule".to_owned()));
        }
        let mut spec = JobSpec::new(Schedule::Interval { every_secs: 3600 }, "");
        self.apply(&mut spec, now, profile_zone)?;
        Ok(spec)
    }

    /// Applies the given fields to `spec`.
    ///
    /// # Errors
    ///
    /// `cron.invalid_schedule`, `cron.invalid_job`.
    pub fn apply(
        &self,
        spec: &mut JobSpec,
        now: Timestamp,
        profile_zone: &TimeZone,
    ) -> Result<(), Error> {
        if self.schedule_text.is_some() {
            return Err(Error::new(
                &codes::CRON_INVALID_SCHEDULE,
                "natural-language schedules (schedule_text, --nl) are not available in this \
                 build: give the schedule in the grammar ('every 30m', 'weekdays at 9:30', \
                 a cron expression, 'tomorrow at 9am')",
            ));
        }
        set(&mut spec.timezone, self.timezone.as_ref());
        if let Some(text) = &self.schedule {
            let zone = spec.zone(profile_zone).map_err(invalid)?;
            spec.schedule = Schedule::parse(text, now, &zone)?;
        }
        self.apply_payload(spec);
        self.apply_routing(spec);
        if let Some(memory) = &self.memory {
            spec.memory = MemoryMode::parse(memory)?;
        }
        if let Some(grace) = &self.grace {
            spec.grace_secs = Some(limit("grace", grace)?);
        }
        if let Some(idle) = &self.idle {
            spec.timeout.idle_secs = Some(limit("idle timeout", idle)?);
        }
        if let Some(wall) = &self.wall {
            spec.timeout.wall_secs = Some(limit("wall timeout", wall)?);
        }
        Ok(())
    }

    fn apply_payload(&self, spec: &mut JobSpec) {
        if let Some(prompt) = &self.prompt {
            spec.prompt.clone_from(prompt);
        }
        if let Some(skills) = &self.skills {
            spec.skills.clone_from(skills);
        }
        set(&mut spec.script, self.script.as_ref());
        if let Some(no_agent) = self.no_agent {
            spec.no_agent = no_agent;
        }
        if let Some(toolsets) = &self.toolsets {
            spec.toolsets = Some(toolsets.clone());
        }
        if self.repeat.is_some() {
            spec.repeat_times = self.repeat;
        }
        match &self.pin {
            Some(Pin::Set { model, provider }) => {
                spec.model = Some(model.clone());
                spec.provider.clone_from(provider);
            }
            Some(Pin::Release) => {
                spec.model = None;
                spec.provider = None;
            }
            None => {}
        }
        set(&mut spec.reasoning_effort, self.reasoning_effort.as_ref());
        set(&mut spec.workdir, self.workdir.as_ref());
        if let Some(secrets) = &self.script_secrets {
            spec.script_secrets = list(secrets);
        }
        set(&mut spec.interpreter, self.interpreter.as_ref());
        set(&mut spec.monitor_script, self.monitor_script.as_ref());
        set(&mut spec.monitor_url, self.monitor_url.as_ref());
    }

    fn apply_routing(&self, spec: &mut JobSpec) {
        if let Some(deliver) = &self.deliver {
            spec.deliver = list(deliver);
        }
        if let Some(failure) = &self.failure_deliver {
            spec.failure_deliver = list(failure);
        }
        if self.attach_to_session.is_some() {
            spec.attach_to_session = self.attach_to_session;
        }
        if let Some(sources) = &self.context_from {
            let keep_self = spec.continuity();
            spec.context_from = list(sources);
            if keep_self && !spec.context_from.iter().any(|c| c == SELF) {
                spec.context_from.push(SELF.to_owned());
            }
        }
        match self.continuity {
            Some(true) if !spec.continuity() => spec.context_from.push(SELF.to_owned()),
            Some(false) => spec.context_from.retain(|c| c != SELF),
            _ => {}
        }
    }

    /// Whether the fields change when the job runs (an edit then needs a
    /// new echo and confirmation).
    #[must_use]
    pub fn reschedules(&self) -> bool {
        self.schedule.is_some() || self.schedule_text.is_some() || self.timezone.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{Fields, Pin};
    use crate::job::JobSpec;
    use crate::schedule::Schedule;

    fn spec() -> JobSpec {
        JobSpec::new(Schedule::Interval { every_secs: 3600 }, "x")
    }

    #[test]
    fn continuity_toggles_only_the_self_entry() {
        let mut job = spec();
        let now = jiff::Timestamp::now();
        let zone = jiff::tz::TimeZone::UTC;
        Fields {
            context_from: Some(vec!["collector".into()]),
            continuity: Some(true),
            ..Fields::default()
        }
        .apply(&mut job, now, &zone)
        .unwrap();
        assert_eq!(job.context_from, ["collector", "self"]);
        Fields {
            continuity: Some(false),
            ..Fields::default()
        }
        .apply(&mut job, now, &zone)
        .unwrap();
        assert_eq!(job.context_from, ["collector"]);
    }

    #[test]
    fn empty_values_clear_and_pins_release() {
        let mut job = spec();
        let now = jiff::Timestamp::now();
        let zone = jiff::tz::TimeZone::UTC;
        Fields {
            workdir: Some("/srv".into()),
            failure_deliver: Some(vec!["telegram".into()]),
            pin: Some(Pin::Set {
                model: "big-model".into(),
                provider: Some("mock".into()),
            }),
            ..Fields::default()
        }
        .apply(&mut job, now, &zone)
        .unwrap();
        assert!(job.pinned());
        Fields {
            workdir: Some(String::new()),
            failure_deliver: Some(vec![String::new()]),
            pin: Some(Pin::Release),
            ..Fields::default()
        }
        .apply(&mut job, now, &zone)
        .unwrap();
        assert_eq!(job.workdir, None);
        assert!(job.failure_deliver.is_empty());
        assert!(!job.pinned());
    }
}

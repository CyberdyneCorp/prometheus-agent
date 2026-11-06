//! The editable fields of a job, as the `cron` CLI flags and the
//! `cronjob` tool arguments give them, applied to a definition.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};

use crate::duration;
use crate::job::{JobSpec, MemoryMode};
use crate::schedule::Schedule;

/// Fields to set; `None` leaves a field as it is.
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
    /// Pinned model.
    pub model: Option<String>,
    /// Pinned provider.
    pub provider: Option<String>,
    /// Pinned IANA zone.
    pub timezone: Option<String>,
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
        if let Some(zone) = &self.timezone {
            spec.timezone = Some(zone.clone()).filter(|z| !z.is_empty());
        }
        if let Some(text) = &self.schedule {
            let zone = spec.zone(profile_zone).map_err(invalid)?;
            spec.schedule = Schedule::parse(text, now, &zone)?;
        }
        self.apply_payload(spec);
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
        let set = |field: &mut Option<String>, value: &Option<String>| {
            if let Some(value) = value {
                *field = Some(value.clone()).filter(|v| !v.is_empty());
            }
        };
        if let Some(prompt) = &self.prompt {
            spec.prompt.clone_from(prompt);
        }
        if let Some(skills) = &self.skills {
            spec.skills.clone_from(skills);
        }
        set(&mut spec.script, &self.script);
        if let Some(no_agent) = self.no_agent {
            spec.no_agent = no_agent;
        }
        if let Some(deliver) = &self.deliver {
            spec.deliver.clone_from(deliver);
        }
        if let Some(toolsets) = &self.toolsets {
            spec.toolsets = Some(toolsets.clone());
        }
        if self.repeat.is_some() {
            spec.repeat_times = self.repeat;
        }
        set(&mut spec.model, &self.model);
        set(&mut spec.provider, &self.provider);
    }

    /// Whether the fields change when the job runs (an edit then needs a
    /// new echo and confirmation).
    #[must_use]
    pub fn reschedules(&self) -> bool {
        self.schedule.is_some() || self.schedule_text.is_some() || self.timezone.is_some()
    }
}

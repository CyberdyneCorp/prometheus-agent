//! Job definitions, states and status literals (spec: scheduled-jobs, Job
//! record and storage; Job states and status literals).

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};
use pmt_store::cron::JobRow;
use serde::{Deserialize, Serialize};

use crate::schedule::Schedule;
use crate::target::JobOrigin;

/// What an unattended run may do with memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMode {
    /// Snapshot in the prompt, no memory write tools.
    #[default]
    ReadOnly,
    /// Snapshot and the memory tool.
    ReadWrite,
    /// Neither.
    Off,
}

impl MemoryMode {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
            Self::Off => "off",
        }
    }

    /// Parses `read_only`, `read_write` or `off`.
    ///
    /// # Errors
    ///
    /// `cron.invalid_job`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        match text {
            "read_only" => Ok(Self::ReadOnly),
            "read_write" => Ok(Self::ReadWrite),
            "off" => Ok(Self::Off),
            other => Err(Error::new(
                &codes::CRON_INVALID_JOB,
                format!("unknown memory mode '{other}' (expected read_only, read_write or off)"),
            )),
        }
    }
}

/// Per-job run limits; unset values use the `cron.*` defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeout {
    /// Seconds without tokens or tool activity (`0` = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u64>,
    /// Seconds in all (`0` = unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_secs: Option<u64>,
}

/// A job's definition (the `spec` column).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    /// When it fires.
    pub schedule: Schedule,
    /// The IANA zone the schedule is pinned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The prompt.
    #[serde(default)]
    pub prompt: String,
    /// Skills preloaded into the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// A script under `<profile>/scripts/`, relative to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The script is the whole job.
    #[serde(default)]
    pub no_agent: bool,
    /// Delivery targets.
    #[serde(default)]
    pub deliver: Vec<String>,
    /// Toolsets of the run (`Some([])` = no tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<String>>,
    /// The memory mode.
    #[serde(default)]
    pub memory: MemoryMode,
    /// Explicit grace in seconds (0 to 86400, applied exactly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
    /// Run limits.
    #[serde(default)]
    pub timeout: Timeout,
    /// Executions before the job completes (`None` = unlimited, 1 for a
    /// one-shot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_times: Option<u32>,
    /// A pinned model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A pinned provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

impl JobSpec {
    /// A spec running `prompt` on `schedule`, delivering locally.
    #[must_use]
    pub fn new(schedule: Schedule, prompt: impl Into<String>) -> Self {
        Self {
            schedule,
            timezone: None,
            prompt: prompt.into(),
            skills: Vec::new(),
            script: None,
            no_agent: false,
            deliver: Vec::new(),
            toolsets: None,
            memory: MemoryMode::ReadOnly,
            grace_secs: None,
            timeout: Timeout::default(),
            repeat_times: None,
            model: None,
            provider: None,
        }
    }

    /// The executions limit: the spec's, else 1 for a one-shot.
    #[must_use]
    pub fn repeat_limit(&self) -> Option<u32> {
        self.repeat_times
            .or_else(|| (!self.schedule.recurring()).then_some(1))
    }

    /// The default name: the prompt's first 50 characters, else the first
    /// skill, else the script's file name.
    #[must_use]
    pub fn default_name(&self) -> String {
        let prompt = self.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        if !prompt.is_empty() {
            return prompt.chars().take(50).collect();
        }
        if let Some(skill) = self.skills.first() {
            return skill.clone();
        }
        self.script
            .as_deref()
            .map(|s| s.rsplit(['/', '\\']).next().unwrap_or(s).to_owned())
            .unwrap_or_default()
    }

    /// The zone the schedule runs in: the job pin, else `profile_zone`.
    ///
    /// # Errors
    ///
    /// The pinned zone is not in the time zone database (the message
    /// names it).
    pub fn zone(&self, profile_zone: &TimeZone) -> Result<TimeZone, String> {
        match self.timezone.as_deref() {
            Some(name) => TimeZone::get(name).map_err(|_| format!("unknown time zone '{name}'")),
            None => Ok(profile_zone.clone()),
        }
    }
}

macro_rules! literals {
    ($(#[$doc:meta])* $name:ident { $($(#[$vdoc:meta])* $variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vdoc])* $variant),+
        }

        impl $name {
            /// Every literal.
            pub const ALL: &[Self] = &[$(Self::$variant),+];

            /// The stored literal.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Parses a stored literal.
            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.as_str() == text)
            }
        }
    };
}

literals!(
    /// `cron_jobs.state`.
    JobState {
        /// Waiting for its next occurrence.
        Scheduled = "scheduled",
        /// Paused by a person.
        Paused = "paused",
        /// A run executes.
        Running = "running",
        /// Its repeat limit was reached or its one instant ran.
        Completed = "completed",
        /// Its next instant cannot be computed.
        Error = "error",
    }
);

literals!(
    /// The terminal status of a run (and a job's `last_status`).
    Status {
        /// Ran and delivered.
        Ok = "ok",
        /// The answer was `[SILENT]` (or a no-agent script printed nothing).
        Silent = "silent",
        /// Failed.
        Failed = "failed",
        /// Interrupted by a timeout.
        TimedOut = "timed_out",
        /// The preflight found something missing.
        BlockedConfig = "blocked_config",
        /// Deliveries wait in the outbox.
        DeliveryQueued = "delivery_queued",
        /// A delivery failed.
        DeliveryFailed = "delivery_failed",
        /// The occurrence was not run.
        SkippedLate = "skipped_late",
    }
);

/// A stored job: its row and parsed definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// The row.
    pub row: JobRow,
    /// The definition.
    pub spec: JobSpec,
    /// Where it was created.
    pub origin: Option<JobOrigin>,
}

impl Job {
    /// Parses a stored row.
    ///
    /// # Errors
    ///
    /// `cron.invalid_job` for a row whose JSON does not parse.
    pub fn from_row(row: JobRow) -> Result<Self, Error> {
        let spec = serde_json::from_str(&row.spec).map_err(|e| {
            Error::new(
                &codes::CRON_INVALID_JOB,
                format!("job {} has an unreadable definition: {e}", row.id),
            )
        })?;
        let origin = row
            .origin
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        Ok(Self { row, spec, origin })
    }

    /// The job id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.row.id
    }

    /// The state.
    #[must_use]
    pub fn state(&self) -> JobState {
        JobState::parse(&self.row.state).unwrap_or(JobState::Error)
    }

    /// The next occurrence.
    #[must_use]
    pub fn next_run(&self) -> Option<Timestamp> {
        self.row.next_run_at.map(crate::schedule::at_ms)
    }
}

/// A new 12-hex-character job id.
#[must_use]
pub fn new_id() -> String {
    let mut bytes = [0_u8; 6];
    if getrandom::fill(&mut bytes).is_err() {
        // A clock-derived id is still unique enough for one profile.
        let nanos = Timestamp::now().as_nanosecond().to_le_bytes();
        bytes.copy_from_slice(&nanos[..6]);
    }
    hex(&bytes)
}

/// Lower-case hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Serializes a spec for the `spec` column.
#[must_use]
pub fn spec_json(spec: &JobSpec) -> String {
    serde_json::to_string(spec).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{JobSpec, JobState, Status, new_id};
    use crate::schedule::Schedule;

    #[test]
    fn literals_and_defaults() {
        assert_eq!(JobState::ALL.len(), 5);
        assert_eq!(Status::parse("skipped_late"), Some(Status::SkippedLate));
        assert_eq!(Status::parse("done"), None);
        let id = new_id();
        assert_eq!(id.len(), 12);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        let mut spec = JobSpec::new(Schedule::Interval { every_secs: 60 }, "");
        spec.script = Some("checks/disk.sh".into());
        assert_eq!(spec.default_name(), "disk.sh");
        spec.prompt = "x".repeat(80);
        assert_eq!(spec.default_name().len(), 50);
        assert_eq!(spec.repeat_limit(), None);
        assert_eq!(
            JobSpec::new(Schedule::Once { at_ms: 0 }, "x").repeat_limit(),
            Some(1)
        );
    }
}

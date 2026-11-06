//! The `cron` subcommands and their flags.

use clap::{Args, Subcommand};

/// `prometheus cron` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum CronCommand {
    /// The jobs: id, name, schedule, state, next run and last status.
    List {
        /// Include completed jobs.
        #[arg(long)]
        all: bool,
    },
    /// Create a job; the schedule is echoed with its next three instants
    /// and saved only after confirmation.
    Add {
        /// `every <duration>`, a cron expression or day/time phrase, an
        /// RFC 3339 time, `at <time>`, `tomorrow at <time>` or `in
        /// <duration>`.
        #[arg(required_unless_present = "schedule_text")]
        schedule: Option<String>,
        /// The job's fields.
        #[command(flatten)]
        flags: JobFlags,
    },
    /// Change a job; a new schedule is echoed and confirmed again.
    Edit {
        /// The job's id or unique name.
        job: String,
        /// A new schedule.
        #[arg(long)]
        schedule: Option<String>,
        /// The fields to change.
        #[command(flatten)]
        flags: JobFlags,
    },
    /// Stop a job from firing until it is resumed.
    Pause {
        /// The job's id or unique name.
        job: String,
        /// Why (shown by `cron list`).
        #[arg(long)]
        reason: Option<String>,
    },
    /// Fire a paused job again, from now.
    Resume {
        /// The job's id or unique name.
        job: String,
    },
    /// Run a job now, in the foreground (its schedule and state are
    /// untouched).
    Run {
        /// The job's id or unique name.
        job: String,
        /// Hand the run to the gateway daemon and print its id instead.
        #[arg(long)]
        queue: bool,
    },
    /// Delete a job (its run records are kept under run retention).
    #[command(visible_alias = "rm")]
    Remove {
        /// The job's id or unique name.
        job: String,
    },
    /// A job's runs, newest first.
    Runs {
        /// The job's id or unique name.
        job: String,
        /// Most runs shown.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only runs with this status (`ok`, `failed`, `timed_out`...).
        #[arg(long)]
        status: Option<String>,
    },
    /// One run in full: times, status, error, session, cost and output.
    ShowRun {
        /// The run id.
        run: String,
    },
    /// Re-execute a recorded run without delivering it.
    Replay {
        /// The run id.
        run: String,
        /// Against the current live provider and harness, printing a diff
        /// of the two final outputs.
        #[arg(long)]
        live: bool,
        /// Deliver the new output (with `--live`).
        #[arg(long, requires = "live")]
        deliver: bool,
    },
    /// Job counts, the next due job and who ticks the profile.
    Status,
    /// Trigger, tick lease, due-but-unfired occurrences, jobs in error,
    /// undelivered outbox rows and every `cron.*` setting; exits 1 when a
    /// job is in error or a delivery outcome is unknown.
    Doctor,
    /// Run one tick in this process (for OS schedulers); does nothing
    /// while a gateway daemon owns scheduling.
    Tick,
}

/// The fields of a job.
#[derive(Debug, Clone, Default, Args)]
pub struct JobFlags {
    /// Display name (default: the prompt's first 50 characters).
    #[arg(long)]
    pub name: Option<String>,
    /// The self-contained prompt each run starts with.
    #[arg(long)]
    pub prompt: Option<String>,
    /// A skill preloaded into each run (repeatable).
    #[arg(long = "skill", value_name = "NAME")]
    pub skills: Vec<String>,
    /// A script in the profile's `scripts/` directory; its output joins
    /// the prompt.
    #[arg(long)]
    pub script: Option<String>,
    /// The script is the whole job: its output is delivered as is.
    #[arg(long)]
    pub no_agent: bool,
    /// Where output goes (repeatable): `local`, `origin`, `none`,
    /// `<platform>`, `<platform>:<chat_id>[:<thread_id>]`,
    /// `bot_chat[:<profile>]`.
    #[arg(long = "deliver", value_name = "TARGET")]
    pub deliver: Vec<String>,
    /// Toolsets of each run (comma-separated; `none` for no tools).
    #[arg(long, value_delimiter = ',')]
    pub toolsets: Option<Vec<String>>,
    /// `read_only` (default), `read_write` or `off`.
    #[arg(long)]
    pub memory: Option<String>,
    /// How late a run may start (`0s` to `24h`).
    #[arg(long)]
    pub grace: Option<String>,
    /// Interrupt a run idle this long (`0s` = unlimited).
    #[arg(long)]
    pub idle_timeout: Option<String>,
    /// Interrupt a run after this long (`0s` = unlimited).
    #[arg(long)]
    pub wall_timeout: Option<String>,
    /// Runs before the job completes.
    #[arg(long)]
    pub repeat: Option<u32>,
    /// A pinned model.
    #[arg(long)]
    pub model: Option<String>,
    /// A pinned provider.
    #[arg(long)]
    pub provider: Option<String>,
    /// The IANA zone the schedule runs in (default `cron.timezone`).
    #[arg(long = "tz", value_name = "ZONE")]
    pub timezone: Option<String>,
    /// A free-form schedule for the auxiliary model to translate.
    #[arg(long = "nl", value_name = "TEXT")]
    pub schedule_text: Option<String>,
    /// Save without asking.
    #[arg(long)]
    pub yes: bool,
}

impl JobFlags {
    /// The flags as job fields (with `schedule`).
    #[must_use]
    pub fn fields(&self, schedule: Option<&str>) -> pmt_cron::fields::Fields {
        let some = |list: &Vec<String>| (!list.is_empty()).then(|| list.clone());
        pmt_cron::fields::Fields {
            schedule: schedule.map(str::to_owned),
            schedule_text: self.schedule_text.clone(),
            prompt: self.prompt.clone(),
            skills: some(&self.skills),
            script: self.script.clone(),
            no_agent: self.no_agent.then_some(true),
            deliver: some(&self.deliver),
            toolsets: self
                .toolsets
                .clone()
                .map(|t| if t == ["none"] { Vec::new() } else { t }),
            memory: self.memory.clone(),
            grace: self.grace.clone(),
            idle: self.idle_timeout.clone(),
            wall: self.wall_timeout.clone(),
            repeat: self.repeat,
            model: self.model.clone(),
            provider: self.provider.clone(),
            timezone: self.timezone.clone(),
        }
    }
}

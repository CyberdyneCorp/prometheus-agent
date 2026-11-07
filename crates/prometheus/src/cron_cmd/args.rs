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
        /// The prompt (same as `--prompt`).
        #[arg(value_name = "PROMPT", conflicts_with = "prompt")]
        prompt_text: Option<String>,
        /// The job's fields.
        #[command(flatten)]
        flags: JobFlags,
        /// Create it paused: nothing fires until `cron resume`.
        #[arg(long)]
        paused: bool,
        /// Why it is paused (default `Created paused; awaiting operator
        /// approval.`).
        #[arg(long, requires = "paused")]
        paused_reason: Option<String>,
        /// The suggestion this job accepts (`cron suggestions accept`).
        #[arg(long, hide = true)]
        from_suggestion: Option<String>,
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
    /// Failure incidents, newest activity first; `ack <id>` silences one.
    #[command(args_conflicts_with_subcommands = true)]
    Incidents {
        /// Acknowledge an incident.
        #[command(subcommand)]
        action: Option<IncidentAction>,
        /// Only incidents in this state (`detected`, `alerted`, `resolved`,
        /// `closed`).
        #[arg(long)]
        state: Option<String>,
        /// Only incidents of this job (id or name).
        #[arg(long)]
        job: Option<String>,
    },
    /// A job's notepad: `list` (default), `get <key>`, `set <key> <value>`,
    /// `delete <key>`.
    Notepad {
        /// The job's id or unique name.
        job: String,
        /// What to do.
        #[command(subcommand)]
        action: Option<NotepadAction>,
    },
    /// Suggested automations: `list` (default), `accept <n> [--yes]`,
    /// `dismiss <n>`, `catalog` (adds the curated starters), `clear`.
    Suggestions {
        /// What to do.
        #[command(subcommand)]
        action: Option<SuggestionAction>,
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

/// `cron incidents` actions.
#[derive(Debug, Clone, Subcommand)]
pub enum IncidentAction {
    /// Close an incident: its signature stays silent.
    Ack {
        /// The incident id.
        id: String,
    },
}

/// `cron notepad` actions.
#[derive(Debug, Clone, Subcommand)]
pub enum NotepadAction {
    /// Every key and value.
    List,
    /// One value.
    Get {
        /// The key.
        key: String,
    },
    /// Sets a value.
    Set {
        /// The key (at most 128 characters).
        key: String,
        /// The value (at most 16 KiB).
        value: String,
    },
    /// Deletes a key.
    Delete {
        /// The key.
        key: String,
    },
}

/// `cron suggestions` actions.
#[derive(Debug, Clone, Subcommand)]
pub enum SuggestionAction {
    /// The pending suggestions, numbered.
    List,
    /// Create suggestion `n`'s job (its schedule is echoed and confirmed).
    Accept {
        /// Its number in the list.
        n: usize,
        /// Save without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Never offer suggestion `n` again.
    Dismiss {
        /// Its number in the list.
        n: usize,
    },
    /// Add the curated starters (daily briefing, important-mail monitor,
    /// weekly review, workday-start reminder).
    Catalog,
    /// Drop accepted and dismissed suggestions from the inbox (dismissed
    /// keys stay remembered).
    Clear,
}

/// The fields of a job.
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
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
    /// Pin this model.
    #[arg(long)]
    pub model: Option<String>,
    /// The provider of `--model`.
    #[arg(long, requires = "model")]
    pub provider: Option<String>,
    /// Pin the profile's current main model and provider (ignored with
    /// `--model`).
    #[arg(long, conflicts_with = "unpin")]
    pub pin: bool,
    /// Follow `cron.model` (else the main model) again.
    #[arg(long)]
    pub unpin: bool,
    /// Pin a reasoning level: none, minimal, low, medium, high, xhigh or
    /// max (`""` clears).
    #[arg(long, value_name = "LEVEL")]
    pub reasoning_effort: Option<String>,
    /// An existing absolute directory the run works in, loading its
    /// project context files (`""` clears).
    #[arg(long, value_name = "DIR")]
    pub workdir: Option<String>,
    /// Where failure notices go instead of `--deliver` (repeatable; `""`
    /// clears).
    #[arg(long = "failure-deliver", value_name = "TARGET")]
    pub failure_deliver: Vec<String>,
    /// Link deliveries to a conversation a reply continues (`true` or
    /// `false`; unset follows `cron.continuable`).
    #[arg(long, value_name = "BOOL")]
    pub attach_to_session: Option<bool>,
    /// A secret passed to the script's environment (repeatable; `""`
    /// clears).
    #[arg(long = "script-secret", value_name = "NAME")]
    pub script_secrets: Vec<String>,
    /// The absolute interpreter of a `.py` script (`""` clears).
    #[arg(long, value_name = "PATH")]
    pub interpreter: Option<String>,
    /// A monitor script in `scripts/`: the agent runs only when its
    /// output changes (`""` clears).
    #[arg(long, value_name = "SCRIPT")]
    pub monitor_script: Option<String>,
    /// A monitored http(s) URL: the agent runs only when the page changes
    /// (`""` clears).
    #[arg(long, value_name = "URL")]
    pub monitor_url: Option<String>,
    /// A job whose latest output is prepended to the prompt (repeatable;
    /// `""` clears).
    #[arg(long = "context-from", value_name = "JOB")]
    pub context_from: Vec<String>,
    /// Prepend this job's own previous report.
    #[arg(long, conflicts_with = "no_continuity")]
    pub continuity: bool,
    /// Stop prepending this job's own previous report.
    #[arg(long)]
    pub no_continuity: bool,
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
    /// The flags as job fields (with `schedule`); `--pin` freezes the
    /// profile's main model of `scope`.
    ///
    /// # Errors
    ///
    /// `cron.invalid_job` for `--pin` without a main model.
    pub fn fields(
        &self,
        schedule: Option<&str>,
        scope: &pmt_config::ProfileScope,
    ) -> Result<pmt_cron::fields::Fields, pmt_core::error::Error> {
        let some = |list: &Vec<String>| (!list.is_empty()).then(|| list.clone());
        Ok(pmt_cron::fields::Fields {
            schedule: schedule.map(str::to_owned),
            schedule_text: self.schedule_text.clone(),
            prompt: self.prompt.clone(),
            skills: some(&self.skills),
            script: self.script.clone(),
            no_agent: self.no_agent.then_some(true),
            deliver: some(&self.deliver),
            failure_deliver: some(&self.failure_deliver),
            attach_to_session: self.attach_to_session,
            toolsets: self
                .toolsets
                .clone()
                .map(|t| if t == ["none"] { Vec::new() } else { t }),
            memory: self.memory.clone(),
            grace: self.grace.clone(),
            idle: self.idle_timeout.clone(),
            wall: self.wall_timeout.clone(),
            repeat: self.repeat,
            pin: self.pin_change(scope)?,
            reasoning_effort: self.reasoning_effort.clone(),
            timezone: self.timezone.clone(),
            workdir: self.workdir.clone(),
            script_secrets: some(&self.script_secrets),
            interpreter: self.interpreter.clone(),
            monitor_script: self.monitor_script.clone(),
            monitor_url: self.monitor_url.clone(),
            context_from: some(&self.context_from),
            continuity: if self.continuity {
                Some(true)
            } else {
                self.no_continuity.then_some(false)
            },
        })
    }

    fn pin_change(
        &self,
        scope: &pmt_config::ProfileScope,
    ) -> Result<Option<pmt_cron::fields::Pin>, pmt_core::error::Error> {
        use pmt_cron::fields::Pin;
        if let Some(model) = &self.model {
            return Ok(Some(Pin::Set {
                model: model.clone(),
                provider: self.provider.clone(),
            }));
        }
        if self.unpin {
            return Ok(Some(Pin::Release));
        }
        if self.pin {
            return main_model(scope).map(Some);
        }
        Ok(None)
    }
}

/// The profile's current main model and provider as a pin.
///
/// # Errors
///
/// `cron.invalid_job` when the profile names no main model.
pub fn main_model(
    scope: &pmt_config::ProfileScope,
) -> Result<pmt_cron::fields::Pin, pmt_core::error::Error> {
    let model = &scope.config().model;
    let Some(name) = model.default.clone() else {
        return Err(pmt_core::error::Error::new(
            &pmt_core::error::codes::CRON_INVALID_JOB,
            "cannot pin: the profile has no main model (`model.default`)",
        ));
    };
    Ok(pmt_cron::fields::Pin::Set {
        model: name,
        provider: model.provider.clone(),
    })
}

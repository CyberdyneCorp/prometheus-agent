//! `/cron [list|add|edit|pause|resume|run|remove|runs|incidents] ...` on
//! every chat surface (spec: scheduled-jobs, /cron slash command): the
//! arguments of `prometheus cron`, parsed by the same definitions; a job
//! created here records the invoking chat as its origin; saving shows the
//! schedule echo and needs `--yes` or a `yes` reply (the next input of the
//! session; any other input drops the shown job); `/cron run` starts the run in the
//! background and posts its outcome to the same chat; output on
//! messaging platforms is cut to the platform's message limit.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use clap::Parser;
use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_cron::job::Job;
use pmt_cron::ops;
use pmt_cron::target::JobOrigin;
use pmt_store::Origin;

use super::Ctx;
use crate::cron_cmd::{CronCommand, IncidentAction, JobFlags};
use crate::cron_host::{HostRunner, profile_zone, scripts_dir};

/// `/cron` as clap sees it.
#[derive(Debug, Parser)]
#[command(name = "/cron", no_binary_name = true, disable_help_flag = true)]
struct Slash {
    #[command(subcommand)]
    command: Option<CronCommand>,
}

/// The reply that saves the job a `/cron add|edit` echo showed.
pub const CONFIRM_REPLY: &str = "yes";
/// How long a shown echo waits for its reply (as long as the CLI's
/// confirmation tokens).
const CONFIRM_WITHIN: Duration = Duration::from_mins(10);

/// What a session's next input answers.
#[derive(Debug, Clone)]
pub(super) enum Pending {
    /// A shown `/cron add|edit` echo: `yes` runs these words with `--yes`.
    Echo(Vec<String>),
    /// A `/blueprint` question: any input that is not a command answers it.
    Question(super::blueprint::Asking),
}

/// What each session's next input answers, and since when.
type Waiting = Mutex<HashMap<SessionId, (Pending, Instant)>>;

/// The inputs sessions wait for.
fn shown() -> &'static Waiting {
    static SHOWN: OnceLock<Waiting> = OnceLock::new();
    SHOWN.get_or_init(Mutex::default)
}

/// Makes the next input of session `id` answer `pending`.
pub(super) fn wait_for(id: SessionId, pending: Pending) {
    shown()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, (pending, Instant::now()));
}

/// An input answering what its session waited for: the `yes` to a shown
/// echo, or the answer to a `/blueprint` question.
#[derive(Debug)]
pub enum Confirmation {
    /// Save the shown job.
    Echo(Vec<String>),
    /// The answer to a blueprint question.
    Answer(super::blueprint::Asking, String),
}

fn is_reply(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(CONFIRM_REPLY)
}

/// Whether `text` answers what `pending` waits for.
fn answers(pending: &Pending, text: &str) -> bool {
    match pending {
        Pending::Echo(_) => is_reply(text),
        Pending::Question(_) => {
            let text = text.trim();
            !text.is_empty() && !text.starts_with('/')
        }
    }
}

/// Whether `text` answers what session `id` waits for (the `yes` saving
/// a shown echo, the answer to a blueprint question).
#[must_use]
pub fn awaits(id: SessionId, text: &str) -> bool {
    shown()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .is_some_and(|(pending, at)| at.elapsed() <= CONFIRM_WITHIN && answers(pending, text))
}

/// The next input `text` of session `id`: what it answers, when it
/// answers what the session waited for; any input ends the wait.
#[must_use]
pub fn take(id: SessionId, text: &str) -> Option<Confirmation> {
    let (pending, at) = shown()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id)?;
    if at.elapsed() > CONFIRM_WITHIN || !answers(&pending, text) {
        return None;
    }
    Some(match pending {
        Pending::Echo(words) => Confirmation::Echo(words),
        Pending::Question(asking) => Confirmation::Answer(asking, text.trim().to_owned()),
    })
}

/// Acts on `confirmation`: saves the shown job (the shown command again,
/// with `--yes`: it is validated again, so a job that stopped being valid
/// is refused), or takes the blueprint answer and asks the next question.
///
/// # Errors
///
/// As `/cron add|edit` and `/blueprint`.
pub fn confirm(ctx: &Ctx<'_>, confirmation: Confirmation) -> Result<String, Error> {
    match confirmation {
        Confirmation::Echo(mut words) => {
            words.insert(1.min(words.len()), "--yes".to_owned());
            dispatch(ctx, words)
        }
        Confirmation::Answer(asking, text) => super::blueprint::answer(ctx, asking, &text),
    }
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// Splits `args` like a shell: whitespace separates words, single and
/// double quotes group them (a backslash escapes the next character
/// inside double quotes and outside quotes).
pub(super) fn words(args: &str) -> Result<Vec<String>, Error> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = args.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            (_, c) => word.push(c),
        }
    }
    if quote.is_some() {
        return Err(usage("unterminated quote in /cron arguments"));
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

/// `/cron ...`.
///
/// # Errors
///
/// Usage errors, the errors of the job operations.
pub(super) fn cron(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let _ = take(ctx.live.id, "");
    dispatch(ctx, words(args)?)
}

/// Runs `/cron` with `words` (a shown echo waits for its `yes`).
pub(super) fn dispatch(ctx: &Ctx<'_>, words: Vec<String>) -> Result<String, Error> {
    let parsed = Slash::try_parse_from(&words).map_err(|e| usage(e.to_string()))?;
    let command = parsed.command.unwrap_or(CronCommand::List { all: false });
    let text = match run(ctx, command)? {
        Shown::Done(text) => text,
        Shown::Echo(text) => {
            wait_for(ctx.live.id, Pending::Echo(words));
            text
        }
    };
    Ok(fit(ctx, text))
}

/// What a `/cron` command shows: its outcome, or a schedule echo whose job
/// waits for the `yes` reply.
enum Shown {
    Done(String),
    Echo(String),
}

/// Cuts `text` to the platform's message limit with a pointer to the CLI.
fn fit(ctx: &Ctx<'_>, text: String) -> String {
    const MORE: &str = "\n… (truncated: `prometheus cron` shows everything)";
    match ctx.message_limit {
        Some(limit) if text.chars().count() > limit => {
            let keep = limit.saturating_sub(MORE.chars().count());
            let mut cut: String = text.chars().take(keep).collect();
            cut.push_str(MORE);
            cut
        }
        _ => text,
    }
}

/// The chat a job created here delivers back to.
fn origin(ctx: &Ctx<'_>) -> Option<JobOrigin> {
    match ctx.factory.store.session(ctx.live.id).ok()?.origin {
        Origin::Gateway {
            platform,
            chat,
            thread,
            ..
        } => Some(JobOrigin::Chat {
            platform,
            chat,
            thread,
        }),
        _ => None,
    }
}

fn runner(ctx: &Ctx<'_>) -> HostRunner {
    HostRunner::new(
        ctx.scope,
        ctx.factory.launch(),
        Arc::clone(&ctx.factory.store),
    )
}

fn line(job: &Job) -> String {
    format!(
        "{}  {}  {}  {} {}  next {}  last {}",
        job.id(),
        job.row.name,
        job.row.state,
        job.spec.schedule.kind(),
        job.spec.schedule.canonical(),
        job.next_run()
            .map_or_else(|| "-".to_owned(), |t| t.to_string()),
        job.row.last_status.as_deref().unwrap_or("-"),
    )
}

fn run(ctx: &Ctx<'_>, command: CronCommand) -> Result<Shown, Error> {
    let now = Timestamp::now();
    match command {
        CronCommand::Add {
            schedule,
            prompt_text,
            mut flags,
            paused,
            paused_reason,
            from_suggestion,
        } => {
            if prompt_text.is_some() {
                flags.prompt = prompt_text;
            }
            let paused = paused.then(|| paused_reason.unwrap_or_default());
            let shown = add(ctx, schedule.as_deref(), &flags, paused, now)?;
            if let (Shown::Done(_), Some(key)) = (&shown, from_suggestion) {
                pmt_cron::suggest::accepted(&ctx.factory.store, &key)?;
            }
            Ok(shown)
        }
        CronCommand::Edit {
            job,
            schedule,
            flags,
        } => edit(ctx, &job, schedule.as_deref(), &flags, now),
        other => operate(ctx, other, now).map(Shown::Done),
    }
}

/// The commands that never wait for a confirmation.
fn operate(ctx: &Ctx<'_>, command: CronCommand, now: Timestamp) -> Result<String, Error> {
    let store = &ctx.factory.store;
    match command {
        CronCommand::List { all } => list(ctx, all),
        CronCommand::Pause { job, reason } => {
            let job = ops::pause(store, &job, reason.as_deref(), now)?;
            Ok(format!("paused job {} ({})", job.id(), job.row.name))
        }
        CronCommand::Resume { job } => {
            let job = ops::resume(store, &job, now, &profile_zone(ctx.scope)?)?;
            Ok(format!(
                "resumed job {} ({})\n{}",
                job.id(),
                job.row.name,
                line(&job)
            ))
        }
        CronCommand::Remove { job } => {
            let job = ops::remove(store, &job)?;
            Ok(format!("removed job {} ({})", job.id(), job.row.name))
        }
        CronCommand::Run { job, .. } => run_now(ctx, &job, now),
        CronCommand::Runs { job, limit, status } => runs(ctx, &job, limit, status.as_deref()),
        CronCommand::Incidents { action, state, job } => {
            incidents(ctx, action, state.as_deref(), job, now)
        }
        _ => Err(usage(
            "here /cron offers list, add, edit, pause, resume, run, remove, runs and incidents; \
             use `prometheus cron` for the rest",
        )),
    }
}

fn list(ctx: &Ctx<'_>, all: bool) -> Result<String, Error> {
    let jobs: Vec<Job> = ctx
        .factory
        .store
        .cron_jobs()?
        .into_iter()
        .filter(|r| all || r.state != "completed")
        .filter_map(|r| Job::from_row(r).ok())
        .collect();
    if jobs.is_empty() {
        return Ok("no cron jobs".to_owned());
    }
    Ok(jobs.iter().map(line).collect::<Vec<_>>().join("\n"))
}

fn add(
    ctx: &Ctx<'_>,
    schedule: Option<&str>,
    flags: &JobFlags,
    paused: Option<String>,
    now: Timestamp,
) -> Result<Shown, Error> {
    let runner = runner(ctx);
    let scripts = scripts_dir(ctx.scope);
    let profile = runner.profile(&scripts);
    let origin = origin(ctx);
    let mut spec = flags
        .fields(schedule, ctx.scope)?
        .new_spec(now, &profile.zone)?;
    if spec.deliver.is_empty() {
        spec.deliver = ops::default_deliver(origin.as_ref());
    }
    let echo = ops::check(&profile, &spec, now)?;
    if !flags.yes {
        return Ok(Shown::Echo(format!(
            "{}\nNot saved yet: reply `{CONFIRM_REPLY}` to save this job (or send the same \
             command with --yes).",
            echo.render()
        )));
    }
    let new = ops::NewJob {
        name: flags.name.clone(),
        origin: origin.as_ref(),
        paused,
    };
    let job = ops::create(&profile, spec, new, now)?;
    Ok(Shown::Done(format!(
        "{}\ncreated job {} ({})",
        echo.render(),
        job.id(),
        job.row.name
    )))
}

fn edit(
    ctx: &Ctx<'_>,
    key: &str,
    schedule: Option<&str>,
    flags: &JobFlags,
    now: Timestamp,
) -> Result<Shown, Error> {
    let runner = runner(ctx);
    let scripts = scripts_dir(ctx.scope);
    let profile = runner.profile(&scripts);
    let fields = flags.fields(schedule, ctx.scope)?;
    if fields.reschedules() && !flags.yes {
        let mut spec = ops::find(&ctx.factory.store, key)?.spec;
        fields.apply(&mut spec, now, &profile.zone)?;
        let echo = ops::check(&profile, &spec, now)?;
        return Ok(Shown::Echo(format!(
            "{}\nNot saved yet: reply `{CONFIRM_REPLY}` to save the new schedule (or send the \
             same command with --yes).",
            echo.render()
        )));
    }
    let (job, _) = ops::update(
        &profile,
        key,
        flags.name.as_deref(),
        |spec| fields.apply(spec, now, &profile.zone),
        now,
    )?;
    Ok(Shown::Done(format!(
        "updated job {}\n{}",
        job.id(),
        line(&job)
    )))
}

/// Starts the run in the background; its outcome is posted to the chat.
fn run_now(ctx: &Ctx<'_>, key: &str, now: Timestamp) -> Result<String, Error> {
    let how = ops::QueueRun {
        exclusive: true,
        ..ops::QueueRun::default()
    };
    let (job, run) = ops::queue_run(&ctx.factory.store, key, now, &how)?;
    let runner = runner(ctx);
    let post = Arc::clone(&ctx.post);
    let started = format!(
        "started run {} of job {} ({})",
        run.id,
        job.id(),
        job.row.name
    );
    pmt_core::spawn::spawn(async move {
        let text = match runner.execute(&job, &run).await {
            Ok(done) => {
                let mut text = format!("cron run {} of {}: {}", done.id, job.row.name, done.status);
                if let Some(error) = &done.error {
                    let _ = write!(text, " ({error})");
                }
                if let Some(output) = &done.output {
                    let _ = write!(text, "\n\n{}", output.trim_end());
                }
                text
            }
            Err(err) => format!("cron run {} failed to record: {err}", run.id),
        };
        post(text);
    });
    Ok(started)
}

fn runs(ctx: &Ctx<'_>, key: &str, limit: usize, status: Option<&str>) -> Result<String, Error> {
    let job = ops::find(&ctx.factory.store, key)?;
    let runs = ctx.factory.store.cron_runs(job.id(), status, limit)?;
    if runs.is_empty() {
        return Ok(format!("job {} has no runs", job.id()));
    }
    Ok(runs
        .iter()
        .map(|r| {
            format!(
                "{}  {}  {}{}",
                r.id,
                pmt_cron::schedule::at_ms(r.scheduled_at),
                r.status,
                r.error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn incidents(
    ctx: &Ctx<'_>,
    action: Option<IncidentAction>,
    state: Option<&str>,
    job: Option<String>,
    now: Timestamp,
) -> Result<String, Error> {
    let store = &ctx.factory.store;
    if let Some(IncidentAction::Ack { id }) = action {
        let row = store.cron_ack_incident(&id, now.as_millisecond())?;
        return Ok(format!(
            "acknowledged incident {} of job {}",
            row.id, row.job_name
        ));
    }
    let job_id = job
        .map(|key| ops::find(store, &key).map(|j| j.id().to_owned()))
        .transpose()?;
    let rows = store.cron_incidents(state, job_id.as_deref())?;
    if rows.is_empty() {
        return Ok("no cron incidents".to_owned());
    }
    Ok(rows
        .iter()
        .map(|r| {
            format!(
                "{}  {}  {}  x{}  {}",
                r.id, r.state, r.job_name, r.occurrences, r.error
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::words;

    #[test]
    fn quoted_words_stay_together() {
        assert_eq!(
            words(r#"add "every day at 18:00" "summarize today's commits" --yes"#).unwrap(),
            [
                "add",
                "every day at 18:00",
                "summarize today's commits",
                "--yes"
            ]
        );
        assert_eq!(words("pause 'a b' ''").unwrap(), ["pause", "a b", ""]);
        assert!(words("add \"open").is_err());
    }
}

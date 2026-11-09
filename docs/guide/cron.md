# Scheduled jobs

A scheduled job runs a self-contained prompt, skills or a script on a
schedule and delivers the result to a messaging chat, keeps it locally, or
discards it. Jobs live in each profile's `state.db`; the gateway daemon
fires them (or `prometheus cron tick` from an OS scheduler), every
occurrence runs at most once, and every run is recorded.

```text
$ prometheus cron add "weekdays at 9am" --prompt "Summarise yesterday's merged pull requests" --deliver telegram
cron 0 9 * * 1-5 (Europe/Lisbon)
  next: Mon 2026-11-09 09:00 WET (2026-11-09T09:00:00Z)
  next: Tue 2026-11-10 09:00 WET (2026-11-10T09:00:00Z)
  next: Wed 2026-11-11 09:00 WET (2026-11-11T09:00:00Z)
Save this job? [y/N] y
created job 623658e76915 (Summarise yesterday's merged pull requests)
```

## Schedules

Every schedule parses to exactly one kind; nothing is guessed:

| Kind | Forms |
|---|---|
| `interval` | `every 30m`, `every 2h`, `every 1d` (units `s`, `m`, `h`, `d`, `w`; at least 60 s) |
| `cron` | a five-field expression (`0 9 * * 1-5`) or an exact phrase: `every monday 9am`, `weekdays at 9:30`, `weekends at 10am`, `every day at 18:00`, `daily at noon` |
| `once` | an RFC 3339 time, `at 9am` (the next 9 am), `at 2026-12-01 07:30`, `tomorrow at 18:00`, `in 30m` |

A bare duration is refused: `ambiguous schedule '30m': use 'every 30m' for
a recurring interval or 'in 30m' for a one-shot`. Anything else outside the
grammar is refused with `invalid schedule '<input>'` and the accepted forms.

```console
$ prometheus cron add "30m" --prompt "check the disks" --yes # exits 65
error: ambiguous schedule '30m': use 'every 30m' for a recurring interval or 'in 30m' for a one-shot
$ prometheus cron add "every 10s" --prompt "check the disks" --yes # exits 65
error: interval must be at least 60s
$ prometheus cron list
no cron jobs
```

Schedules run in the job's zone (`--tz`), else `cron.timezone`, else the
host's. Intervals count from the previous scheduled instant, so a long run
does not shift the next one. A wall-clock time skipped by a daylight-saving
jump fires at the first valid instant after it; a time repeated when clocks
go back fires once, at its first occurrence.

Every create and schedule change shows the canonical echo (kind,
canonical text, zone and the next three instants in local time and UTC) and
saves only after you answer `y`. Without a terminal, pass `--yes`, or the
command refuses with exit 4.

## What a job runs

- `--prompt`: a self-contained prompt. Each run is a new session
  (`source = cron`) with no earlier conversation, the job's skills
  (`--skill`, repeatable) preloaded, and its system prompt, tools and
  memory snapshot fixed at the start.
- `--script <path>`: a script inside `<profile>/scripts/` (a path that
  resolves outside, by `..`, an absolute path or a symbolic link, is
  refused). It runs under the profile's sandbox from
  `<profile>/cron/workspace`; its standard output (up to 64 KiB) is added
  to the prompt under `## Script output`. A non-zero exit fails the run with
  the last 4 KiB of its standard error.
- `--no-agent` (with `--script`): the script is the whole job; its output
  is delivered as it is, and empty output delivers nothing.

Toolsets (`--toolsets web,file`, `none` for no tools) default to
`cron.default_toolsets`, else the cron surface's default
(`toolsets.surfaces.cron`). `clarify` and every other tool that needs a
person are never offered, nor is [`cronjob`](../reference/tools.md#cronjob) unless
`cron.allow_agent_scheduling = true`. Tool calls that need an approval are
decided by `approvals.cron_mode` (`deny` by default; permanent grants still
apply); a denied call is a tool error the run carries on with.

`--memory` is `read_only` (default: the memory snapshot is in the prompt,
but no memory tool), `read_write` or `off`.

Before a run, the job's prompt (and, at run time, the prompt with its skill
bodies and script output) is scanned for injection and concealment
directives, secret-exfiltration patterns and invisible Unicode: a create or
edit is refused with `prompt rejected: <rule>`, a run fails with
`unsafe prompt: <rule>`.

With `cron.preflight = true` (default) each run first checks that the
provider credential resolves, the skills are installed and the delivery
platforms are enabled for the profile. A failure is recorded
`blocked_config` with the missing item, makes no model call, and alerts the
job's chats once until a later preflight passes.

## Delivery

`--deliver` (repeatable) takes `local` (keep the output only), `origin`
(the chat the job was created from), `none`, `<platform>` (its home
channel), `<platform>:<chat_id>[:<thread_id>]` or `bot_chat[:<profile>]`.
The default is `origin` for a job created from a chat, else `local`.
Deliveries go through the job's own profile's adapters: a platform the
profile does not enable fails with `<platform> not configured for profile
<name>` rather than borrowing another profile's adapter.

Messaging deliveries are queued in a durable outbox and sent by the daemon
at most once: transient failures are retried three times (30 s, 60 s,
120 s); a send interrupted by a crash is marked `unknown` and never
repeated. The outcome is written back to the run.

Delivered text is framed as `⏰ <job name>`, the content and a footer
`job <id> · prometheus cron pause <id>` (`cron.wrap_response = false` turns
the frame off; no-agent output is never framed), with known secrets
redacted. An answer that is exactly `[SILENT]` delivers nothing; the run is
recorded `silent` with its output kept.

## When runs happen

The gateway daemon ticks every served profile under its own configuration
every `cron.tick_interval_secs` (default 30) and at the earliest next run,
holding the profile's tick lease so two processes never tick it at once.
Nothing is dispatched while the emergency stop (`prometheus pause`) is on;
jobs stay due until it is lifted.

Without a daemon, schedule `prometheus cron tick` from launchd, a systemd
timer or Task Scheduler: it runs one tick in its own process, and exits 0
with `tick skipped: gateway daemon owns scheduling (pid <pid>)` while a
daemon holds the lease. Its messaging deliveries wait in the outbox for the
daemon; `local` ones complete at once.

An occurrence fired within 60 s is `on_time`, within the job's grace
`late`, beyond it `missed`. The grace is `--grace` (`0s` to `24h`, applied
exactly), else half the period clamped to 2 minutes – 2 hours (2 minutes
for a one-shot). After the host slept, missed occurrences of a job without
`--grace` collapse into one catch-up run (`cron.catch_up_missed`); with an
explicit grace they are recorded as one `skipped_late` run with their
count. A missed one-shot is recorded `skipped_late` and completes.

Each occurrence is claimed once: a run row keyed by (job, instant) and the
job's next run are written together before anything executes. A run whose
process died is recorded `failed` / `interrupted` by the next scheduler
start and is never fired again.

At most `cron.max_parallel_jobs` runs execute at once per profile; an
occurrence that comes due while the job's previous run still runs is
recorded `skipped_late` (`previous run still active`). An agent run with
no tokens or tool activity for `cron.agent_idle_timeout_secs` (default
600) or running past `cron.max_run_secs` (default 3600) is interrupted and
recorded `timed_out` (`reason` `idle` or `wall`); `--idle-timeout` and
`--wall-timeout` override them per job (`0s` = unlimited). A script is
killed after `cron.script_timeout_secs`. None of these come from
environment variables.

## States and history

A job is `scheduled`, `paused`, `running`, `completed` (its `--repeat`
count was reached, or its one-shot ran) or `error` (its next instant cannot
be computed, for example a time zone that no longer exists; it stays
listed). Runs end `ok`, `silent`, `failed`, `timed_out`, `blocked_config`,
`delivery_queued`, `delivery_failed` or `skipped_late`.

| Command | |
|---|---|
| `cron list [--all]` | jobs (`--all` adds completed ones) |
| `cron add <schedule> ...` / `cron edit <job> ...` | create / change (`--schedule` re-echoes) |
| `cron pause <job> [--reason]` / `cron resume <job>` | resume recomputes the next run from now; a past one-shot is refused |
| `cron run <job> [--queue]` | run now in the foreground (state and schedule untouched), or hand it to the daemon |
| `cron remove <job>` (`rm`) | delete (runs are kept) |
| `cron runs <job> [--status S] [--limit N]` | runs, newest first, with duration, cost and session |
| `cron show-run <run>` | one run in full, output included |
| `cron replay <run> --live [--deliver]` | re-run the recorded inputs against the current provider and harness; prints a diff of the outputs |
| `cron status` / `cron doctor` | counts and lease / health report |
| `cron tick` | one headless tick |

A job is named by its id or its unique name; a name several jobs share
exits 2 listing their ids. Run records are kept for
`cron.run_retention_secs` (90 days; `0` forever), completed jobs for
`cron.completed_retention_secs` (7 days; `0` never deleted). Outputs over
64 KiB are kept in the blob store.

`cron doctor` reports the trigger in effect (and why it differs from
`cron.provider`: the `managed` scheduler is not in this build, so it falls
back to `builtin` with a warning at every daemon start), the tick lease
holder, due-but-unfired occurrences, jobs in `error` or `blocked_config`,
undelivered outbox rows and every `cron.*` setting with its source. It
exits 1 when a job is in `error` or a delivery outcome is `unknown`.

## From a chat: the cronjob tool

In interactive sessions (CLI, TUI, desktop, messaging chats) the agent can
manage jobs with [`cronjob`](../reference/tools.md#cronjob) (`create`, `list`, `update`, `pause`, `resume`,
`remove`, `run`, `runs`). A create or schedule change first answers
`needs_confirmation` with the echo and a `confirm_token` valid for 10
minutes; the job is saved only when the agent repeats the call with the
token after you agreed. A job created from a messaging chat remembers it,
so `deliver = origin` posts back to that chat and thread. Scheduled runs,
one-shot runs and the API do not get the tool.

Its fields are those of `cron add` except the model: the agent may only
set `pinned` (freeze or release the current main model, when you ask).
`run` starts the job in the background and answers `{status: "started",
run_id}`; the outcome (status, targets, next run, an excerpt) arrives
later as a `[cron run finished]` message. Its optional `prompt` is added
to that run only, under `## Run Context`.

The token is the agent's own to repeat, so it is no human approval, and a
job the agent sets up never reaches past its session (`cron.beyond_session`
otherwise): its `toolsets` stay within the session's tools, each
`script_secrets` name needs the session's `secrets.read:<NAME>` grant, and
its `interpreter` is never a file the session can write. A person can
widen a job with `prometheus cron add|edit`.

## /cron in a chat

`/cron list|add|edit|pause|resume|run|remove|runs|incidents ...` takes the
arguments of `prometheus cron` on every chat surface. A job added from a
chat records it as its origin, so `deliver = origin` posts back to that
chat and thread. Saving shows the echo and waits: reply `yes` (your next
message in that chat or session) to save the job, or pass `--yes` up front
(`/cron add "every day at 18:00" "summarize today's commits" --yes`). Any
other reply, or ten minutes without one, drops the shown job. `/cron run`
posts the outcome in the chat when the run ends.

## Where and with which model a run works

- `--workdir /abs/dir` (an existing absolute directory, else exit 2): the
  run's terminal, file and code tools work there, the sandbox may write
  there, and its `AGENTS.md` / `CLAUDE.md` / `.cursorrules` are loaded.
  Without it a run is detached and loads no project context.
  `cron doctor` reports a workdir that disappeared. Through the `cronjob`
  tool the model may only pick a workdir inside its own session's
  workspace (its confirmation token is no human approval); anything wider
  is refused (`cron.invalid_workdir`) and needs `cron add|edit --workdir`.
  The workdir is saved with its links resolved; a run whose workdir is
  gone or now resolves elsewhere (a link along it was repointed) is
  refused rather than followed.
- The model is the job's pin (`--model m [--provider p]`, or `--pin` to
  freeze the profile's current main model; `--unpin` releases it), else
  `cron.model` / `cron.model_provider`, else the main model at fire time.
  `--reasoning-effort` pins a reasoning level. `cron list` shows pins.
- `--paused [--paused-reason TEXT]` creates the job disabled with nothing
  armed (default reason `Created paused; awaiting operator approval.`);
  `cron resume` arms it.

## Failures

- Failure notices go to `--failure-deliver` targets, else to `deliver`
  (`local` records them only). A final answer whose first line is exactly
  `[CRON_FAILURE]` fails the run with the rest as its error.
- Each failure is an incident keyed by the job and a normalized error
  (lower case, numbers with duration units dropped). Only the first
  failure of an incident is announced, plus one reminder after
  `cron.failure_repeat_alert_secs` (6 h; 0 alerts every time). A success
  resolves the job's incidents. `cron incidents [--state S] [--job J]`
  lists them; `cron incidents ack <id>` silences that error for good.
- After `cron.failure_nudge_threshold` (3) failures in a row a recurring
  job's notice adds `This job has failed <n> runs in a row ...`;
  `cron list` shows `failing <n> runs in a row`.
- A run that never reached the model because of a network or DNS error is
  re-run after 5, 15 and 30 minutes (`cron.retry_unreachable`), without a
  notice, as long as the re-run comes before the next occurrence.
- When every credential of the run's provider is benched until `t` (a
  rate-limit or billing reset, `auth.credentials_benched` before any
  request) and no fallback chain is configured, the job is held: one
  notice (`usage window closed until t`), nothing fires before `t`, then
  the next occurrence after it (a cron-kind job whose window reopens at
  least half a period early re-runs once at `t`). Preflight lets a merely
  benched credential through so the hold applies.

## Gates, monitors, chains and the notepad

- A script whose last stdout line is `{"wakeAgent": false}` skips the
  agent (`silent`, reason `wake_agent_false`); `{"wakeAgent": true,
  "context": {...}}` adds the context as a JSON block. Scripts run with
  only the secrets named by `--script-secret` (never provider credentials
  or a surface's secrets, such as `TELEGRAM_BOT_TOKEN`, whether or not the
  config names them); `.sh` files run with bash, `.py` files with the managed
  Python or `--interpreter`.
- `--monitor-script` or `--monitor-url` (30 s, 1 MiB) runs first each
  tick. The URL is fetched under the egress rules (`security.egress`: its
  mode, allowlist and blocklist; private, loopback and metadata addresses
  are refused unless `allow_private` is set), on every redirect too: unchanged output skips the model (`silent`, reason `no_change`), a
  change adds a `MONITOR CHANGE DETECTED` diff to the prompt.
- `--context-from JOB` prepends the latest output of another job;
  `--continuity` prepends the job's own previous report (quiet runs are
  skipped over).
- `prometheus cron notepad <job> set <key> <value>` keeps notes a run sees
  at the top of its prompt (`## Job notepad`). A run can call it from its
  terminal: the run's sandbox keeps it away from the home, so there the
  command edits a copy of the job's notepad (and only that job's) that is
  saved when the run ends. Limits: keys 128 characters, values 16 KiB, 64 KiB per job
  (exit 65 `notepad limit exceeded`).

## Delivery details

- `cron.delivery.notify` (default true) sends output and notices as final,
  notifying messages, text and media alike, overriding an adapter's quiet
  notification mode and never muted as warnings. With `false` output is
  sent silently where the platform can (Telegram `disable_notification`,
  Discord suppressed notifications) and failure notices are warnings.
  `cron.delivery.threads.<platform> = "<thread>"` sends home-chat
  deliveries to that thread or topic.
- A delivery is verified when the platform returns a message id; otherwise
  `cron list` shows `delivery UNVERIFIED: <target>` and `cron doctor`
  lists it until a verified delivery. An empty answer is never sent
  (`delivery_failed`, `empty payload`).

## Bot Chat deliveries

`bot_chat` (the job's own profile) and `bot_chat:<profile>` (another
profile of the same home; an unknown one is refused when the job is
created) hand the output to that profile's canonical Bot Chat as an
incoming message `[Scheduled: <job name>]`, and that profile takes one turn
under its own configuration. The Bot Chat is created on the first
delivery. The run stays `delivery_queued` until the daemon accepts the
turn, then `ok`, and shows the receipt id (`cron show-run`); a retried
delivery reuses the receipt and never starts a second turn. A target
profile the daemon does not serve fails the delivery (`delivery_failed`)
with no turn anywhere else.

## Continuable deliveries

With `--attach-to-session true` (or `cron.continuable = true` when the job
leaves it unset) a delivery to the job's origin chat or a home channel (or
to the single explicit chat of a job that set `--attach-to-session true`)
starts a conversation a reply continues. Where the platform has threads or
topics, each delivery opens a new one titled with the job's name, whose
session starts with the delivered text as a user turn
`[Cron delivery: <job name>]`: a reply in that thread is answered with the
delivered text in context. On a platform without threads (or a named
thread) the same turn is added to the chat's session. On an adapter that
supports it (Slack), `cron.continuable_surface.<platform> = "in_channel"`
posts top-level and seeds the channel's session instead.

## Suggested automations

The inbox (`/suggestions`, alias `/suggest`, or `prometheus cron
suggestions`) offers jobs you have not set up yet: the curated starters
(`catalog` adds a morning brief, an important-mail monitor, a weekly review
and a workday-start reminder), the `blueprint` block of an installed skill,
a request the background review noticed you keep making, and a starter for
a newly connected account (`prometheus connectors connect`). At most five
wait at once.

```text
/suggestions             numbered list
/suggestions accept 2    shows the schedule echo; reply `yes` (or add --yes)
/suggestions dismiss 1   never offered again
/suggestions catalog     adds the starters
/suggestions clear       drops accepted and dismissed entries
```

Nothing becomes a job until you accept it: accepting runs the same
validated create as `/cron add`, so the schedule echo is shown and the job
delivers to the chat you accepted it in. A dismissed key stays remembered,
so the same suggestion is not offered again. A skill's blueprint that still
needs values asks for them (`prometheus cron suggestions accept` points
you to `/blueprint` for it).

## Blueprints

`/blueprint` (alias `/bp`) lists ready-made automations; `/blueprint
<name> [slot=value ...]` fills one. The name matches a key, a prefix or a
near spelling (`/bp price` is `price-watch`). Each missing value is asked
one question at a time (your next message answers it); then the job's
prompt and schedule echo are shown, and the job is created only after you
reply `yes` (or pass `--yes`). It delivers to the chat you asked from.

```text
/blueprint morning-brief time=08:00 days=weekdays
  -> cron 0 8 * * 1-5 (UTC) ... reply `yes` to save this job
/bp morning-brief time=25:00
  -> invalid value for slot 'time': expected a time of day as HH:MM (00:00-23:59)
```

Slots are typed: `time` (`HH:MM`, `8am`, `6:30pm`), `weekdays`
(`everyday`, `weekdays`, `weekends` or days such as `mon,wed`), `enum` (one
of its options, or any text when not strict) and `text`. The built-ins are
`morning-brief`, `important-mail`, `weekly-review`, `workday-start`,
`custom-reminder`, `evening-winddown`, `news-digest`, `bill-renewal-watch`,
`price-watch`, `competitor-watch`, `habit-checkin`, `hydration-move`,
`meal-plan`, `learn-daily`, `gratitude-journal` and `on-this-day`. Enabled
plugins add theirs as `<plugin>:<key>` and skills as `skill:<key>`; none
replaces a built-in. The dashboard and desktop fill the same slots as a
form through RPC `cron.blueprints.list` and `cron.blueprints.fill`.

A blueprint is data: a key, title, description, category, a
`schedule_template` and a `prompt_template` with `{slot}` placeholders (in
a schedule, `{days}` is a cron day field and `{time.hour}` /
`{time.minute}` the parts of a time), and its slots:

```yaml
blueprint:
  title: Daily digest
  schedule: "{time.minute} {time.hour} * * {days}"
  prompt: Write my daily digest about {topic}
  slots:
    - {name: time, kind: time, default: "18:00"}
    - {name: days, kind: weekdays, default: weekdays}
    - {name: topic, kind: text, question: "Which topic?"}
```

## Webhook-triggered runs

A webhook route can fire a job instead of a turn: `cron_job` names the
job (id or name; checked when the route loads). A verified event renders
the route's `prompt` from the payload and queues one run of the job with
that text as its `## Run Context` (used for that run only, never saved);
the caller gets `202` at once. The job's own delivery, skills and toolsets
apply (the route's `deliver` and `skills` are not used), and a route cannot
set both `cron_job` and `deliver_only`. An event for a paused job, or one
arriving while the job is already queued or running (a burst), is logged
and answered `{"status": "dropped"}`: one run, never two.

```toml
[gateway.platforms.webhook.routes.pr-feedback]
secret_secret = "PR_SECRET"
events = ["pull_request_review"]
cron_job = "pr-review-sweeper"
prompt = "Review {review.state} on pull request {pull_request.number}"
```

## Not in this build yet

- `cron replay <run>` without `--live` (replaying recorded model responses)
  needs the model-call cache and exits 69.
- Natural-language schedules (`--nl`, `schedule_text`) are refused.

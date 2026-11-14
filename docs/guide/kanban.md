# Kanban

Kanban boards let several profiles (and people) share work. People drive
a board from `prometheus kanban` (and later the dashboard and the desktop
app); one dispatcher per Prometheus home claims ready tasks and starts the
assigned profile as a worker, which drives its task through the `kanban`
tools until it completes, asks for review or blocks.

Boards live at the home root, never inside a profile, so every profile
sees the same boards:

```text
<home>/kanban/
  boards/<slug>/board.db      tasks, links, events, runs (SQLite, WAL, 0600)
  boards/<slug>/board.toml    name, review lane default, archived flag
  boards/<slug>/workspaces/   scratch workspaces, one per task
  boards/<slug>/logs/         one log per worker run
  blobs/                      attachments, content-addressed
```

A board named `default` always exists. Other boards are created with
`prometheus kanban boards create <slug>`; slugs are 1-64 lowercase
alphanumerics, `-` or `_`, not starting with `-` or `_`.

```console
$ prometheus kanban list
board 'default' is empty
$ prometheus kanban boards create ops --name "Operations"
created board ops
$ prometheus kanban boards create -- -ops # exits 2
error: invalid board slug '-ops': 1-64 chars, lowercase alphanumerics, '-' or '_', not starting with '-' or '_'
```

## Choosing the board

Every verb works on `--board <slug>`, else the profile's
`kanban.current_board` (set with `prometheus kanban boards switch <slug>`),
else `default`. A board that does not exist, or is archived, is an error
and never silently replaced by another one:

```console
$ prometheus --set kanban.current_board=atm10 kanban list # exits 66
error: board 'atm10' not found (set kanban.current_board or pass --board)
$ prometheus kanban --board ops list
board 'ops' is empty
```

## Tasks

`create` needs a title and assigns an id `t_` + 8 hex characters. A task
whose parents are all done starts `ready`; a task with an open parent
starts `todo` (with a `dependency_wait` event) and is promoted to `ready`
once every parent is done or archived. `--status triage|todo|blocked`
overrides that.

```console
$ prometheus kanban create "Draft the release notes" --assignee default
created <id> (ready) on board default
$ prometheus kanban create "Publish" --workspace dir:../site # exits 65
error: workspace dir must be an absolute path
$ prometheus kanban create "Nightly report" --idempotency-key nightly-2026-10-29
created <id> (ready) on board default
$ prometheus kanban create "Nightly report" --idempotency-key nightly-2026-10-29
exists <id> (ready) on board default
```

Fields: `--body` (the instructions and acceptance criteria), `--assignee`
and `--reviewer` (existing profiles), `--parent` (repeatable),
`--priority` (higher is claimed first), `--workspace`, `--review` /
`--no-review`, `--max-retries`, `--budget-usd`, `--budget-tokens`,
`--artifact` (a file the worker must leave in its workspace, repeatable)
and `--kind agent|eval|evolve`.

Dependencies are links within one board: `kanban link <parent> <child>`.
Self-links and cycles are refused (`linking A → B would create a cycle`);
linking a `ready` child to an open parent sends it back to `todo`;
linking a running child needs its current run id (`--run`).

## The workflow table

Statuses are `triage`, `todo`, `scheduled`, `ready`, `running`, `blocked`,
`review`, `done` and `archived`. Every transition any surface makes is
checked against one typed table, listed in
[the workflow reference](../reference/kanban-workflow.md); anything else
is refused with `illegal transition <from> → <to> for <actor>` (exit 75,
HTTP 409 from the dashboard API).

- **block** takes a kind — `dependency` (back to `todo`), `needs_input`,
  `capability`, `budget` or `transient` — and a reason. Blocks are sticky:
  the promotion pass never lifts them. The same kind blocking again after
  an unblock sends the task to `triage` with `block_loop_detected`.
- **unblock** returns a task to `review` when it was blocked out of
  review, else to `ready` or `todo` by its parents, and resets the failure
  counter.
- **schedule** parks a task until `--at <RFC 3339>` (or until unblocked);
  it wakes with a `woke` event and is never claimed while parked.
- **complete** moves a task to `done` only when every parent is settled,
  the result or summary carries evidence (whitespace is not evidence),
  every declared artifact exists, `created_tasks` names only tasks the run
  created, and no live worker owns it (pass `--run <run-id>` or
  `--force`). Each refusal is recorded in the task's log.
- **review**: with `--review` on the task, or `review = true` in
  `board.toml`, a worker's completion goes to `review`; `approve` moves it
  to `done`, `request-changes --comment` back to `todo`.
- **archive** terminates a running task's worker (recording
  `archive_worker_termination`) and drops its subscriptions; `restore`
  brings a task back to `triage`, `todo` or `ready`.

Lifecycle verbs take several ids and report each one; the command exits 1
when any failed:

```console
$ prometheus kanban archive t_00000000 # exits 1
failed t_00000000: not found
```

## The dispatcher

Exactly one dispatcher runs per home. By default it runs inside the
gateway daemon (`kanban.dispatch = true`, every
`kanban.dispatch_interval_secs`, default 30); `prometheus kanban dispatch
--once` runs one tick and `--loop` a standalone loop when no daemon runs.
A lease row in each board names the holder (60 s, renewed every 20 s);
while another live dispatcher holds it, the command exits 75 with
`dispatcher lease held by <holder>` without claiming anything. A crashed
holder's lease is taken over once it expires (`dispatcher_takeover`).

```console
$ prometheus kanban dispatch --once
default: claimed <n>, skipped_unassigned <n>, skipped_allowlist <n>, capped <n>, reclaimed <n>, blocked <n>
```

Each tick reclaims dead and expired runs, promotes and wakes tasks, then
claims ready tasks in priority then age order. The tick summary
(`claimed`, `skipped_unassigned`, `skipped_allowlist`, `capped`,
`reclaimed`, `blocked`) is an event `prometheus kanban diag` shows.

| Key | Default | Meaning |
|---|---|---|
| `kanban.max_running` | memory / 512 MiB, 2-8 | workers at once across every board |
| `kanban.max_running_per_profile` | 2 | workers at once per assignee |
| `kanban.dispatch_profiles` | unset | claim only these assignees (`[]` claims nothing) |
| `kanban.default_assignee` | unset | assignee of unassigned tasks (else `skipped_unassigned`) |
| `kanban.failure_limit` | 2 | failed runs before a task is blocked (`gave_up`) |
| `kanban.rate_limit_cooldown_secs` | 300 | wait after a rate-limited worker |
| `kanban.claim_ttl_secs` | 900 | claim lease without a heartbeat |
| `kanban.review_dispatch` | true | start `reviewer` profiles on `review` tasks |

A task assigned to a profile that no longer exists is blocked with kind
`capability`. Every failed run (spawn failure, crash, non-zero exit,
expired claim) counts; a worker exiting 78 (credential revoked, model
gone, capability denied) blocks its task at once, and one exiting 75
(rate limited) goes back to `ready` after the cooldown without counting.

## Workers

The dispatcher starts `prometheus kanban work --run <run-id>` under the
assignee profile with a scrubbed environment (`PATH`, the locale and
`PROMETHEUS_HOME` only) and a single-use run ticket on stdin; a replayed
ticket is refused (`run ticket already used`, exit 78). The worker loads
secrets only from its own profile's secrets store, works in the task's
workspace (`scratch`: `<board>/workspaces/<task>/`, deleted after
completion once declared artifacts are attached; `worktree`: a git
worktree of the profile's `terminal.cwd` on `kanban/<task>`; `dir:<path>`)
and heartbeats every minute. A worker is alive only while its pid runs
with the start time recorded at spawn, so a recycled pid is never
signalled; a worker that outlives its run by 120 s is terminated
(`worker_reaped`).

A worker session gets the `kanban` toolset automatically, scoped to its
own board: [`kanban_show`](../reference/tools.md#kanban_show), [`kanban_create`](../reference/tools.md#kanban_create), [`kanban_comment`](../reference/tools.md#kanban_comment),
[`kanban_attach`](../reference/tools.md#kanban_attach), [`kanban_attachments`](../reference/tools.md#kanban_attachments), [`kanban_link`](../reference/tools.md#kanban_link), [`kanban_complete`](../reference/tools.md#kanban_complete),
[`kanban_block`](../reference/tools.md#kanban_block), [`kanban_schedule`](../reference/tools.md#kanban_schedule), [`kanban_heartbeat`](../reference/tools.md#kanban_heartbeat),
[`kanban_request_review`](../reference/tools.md#kanban_request_review) and [`kanban_request_changes`](../reference/tools.md#kanban_request_changes) (not [`kanban_list`](../reference/tools.md#kanban_list)
or [`kanban_unblock`](../reference/tools.md#kanban_unblock)). Other sessions get the toolset only when `kanban`
is in their toolsets, and the run-lifecycle tools refuse there with `not
a kanban worker session`; delegated children get no kanban tools.

`--budget-usd` and `--budget-tokens` limit a task's spend across all its
runs (delegated children included); an exhausted budget stops the run
and blocks the task with kind `budget`. `kanban show` and `kanban runs`
show the spend, and each run names its session.

## History, attachments and notifications

```console
$ prometheus kanban stats
board default: <n> task(s)
```

`kanban log <id>` prints a task's events (every change and every refusal),
`kanban runs <id>` its attempts, `kanban tail [-f]` the board's latest
events. `kanban attach <id> <file>...` stores files in the blob store
(refused above `kanban.attachment_max_bytes`, default 25 MiB, with
`attachment too large: <size> > <limit>`).

`kanban notify subscribe <id> --adapter telegram --chat <chat>` reports
the task's `completed`, `blocked`, `gave_up` and `review_requested` events
to that chat, sent by the gateway daemon through the subscribing profile's
own adapter. A route that profile cannot reach is skipped with one warning
per subscription, never sent through another profile. `kanban gc` purges
subscriptions of tasks done for longer than
`kanban.done_sub_retention_secs`.

## Export and import

`kanban boards export <slug> <file>` writes a versioned archive of a
board's tasks, links, events, runs, comments and attachments; `kanban
boards import <file> [--rename <slug>]` restores it as a new board (an
existing slug is refused without `--rename`), keeping task ids unless
another board of the home already uses one.

## Eval and evolve tasks

`--kind eval --suite smoke` runs the bundled offline smoke suite (the mock
provider: no live provider request) as the assignee, attaches its report
and completes with its summary; other suites need `prometheus eval run`,
which this build does not have yet, and are blocked with kind
`capability`. `--kind evolve` records a harness-evolution task
(`--target-task`, `--mode`, `--budget-k`); its completion always goes to
`review`, never `done`, but its worker needs harness evolution
(`prometheus evolve`), which is not in this build yet, so it is blocked
with kind `capability` naming that.

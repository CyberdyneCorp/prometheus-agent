# Kanban

## Purpose
Kanban is a durable task board that lets several profiles (and humans) collaborate on work: humans drive it from `prometheus kanban`, the dashboard and the desktop app, while a single dispatcher per Prometheus home claims ready tasks and runs the assigned profile as a worker that drives its task through the `kanban` toolset. Boards are shared across profiles on purpose and are the hard isolation boundary for workers; unlike Hermes, exactly one dispatcher per home is guaranteed by a lease, every workflow transition is defined in one typed table, and worker context is passed by a run ticket instead of environment variables. Tasks can also be eval or harness-evolution runs, so the board doubles as the queue for self-improvement work.

## ADDED Requirements

### Requirement: Board storage location
The system SHALL keep boards at the root Prometheus home (not inside a profile) under `<PROMETHEUS_HOME>/kanban/boards/<slug>/` with `board.db` (SQLite, WAL, mode 0600), `board.toml`, `workspaces/` and `logs/`, attachments in the home's content-addressed blob store, and SHALL always provide a board named `default`; board paths SHALL NOT be overridable through environment variables.
**Milestone:** M3

#### Scenario: Default board exists on first use
- **GIVEN** a fresh Prometheus home
- **WHEN** `prometheus kanban list` runs
- **THEN** `<PROMETHEUS_HOME>/kanban/boards/default/board.db` SHALL exist and the command SHALL print an empty board

### Requirement: Board slugs and selection
A board slug SHALL match `^[a-z0-9][a-z0-9_-]{0,63}$` after lower-casing, otherwise be refused with `invalid board slug '<s>': 1-64 chars, lowercase alphanumerics, '-' or '_', not starting with '-' or '_'`; the active board SHALL resolve as `--board`, then `kanban.current_board` in the invoking profile's config, then `default`, and a named board that does not exist or is archived SHALL be an error (`board '<s>' not found`), never a silent fallback.
**Milestone:** M3

#### Scenario: Stale selection is reported
- **GIVEN** `kanban.current_board = "atm10"` and no such board
- **WHEN** `prometheus kanban list` runs
- **THEN** the command SHALL exit 66 with `board 'atm10' not found (set kanban.current_board or pass --board)`

### Requirement: Typed workflow table
The system SHALL define task statuses `triage`, `todo`, `scheduled`, `ready`, `running`, `blocked`, `review`, `done` and `archived` and every allowed transition (actor, from, to, guard) in one workflow table from which the CLI, tools, dashboard API and documentation are generated; any transition not in the table SHALL be refused with `illegal transition <from> → <to> for <actor>`. In particular `review → todo` (request changes) SHALL land in `todo` and be re-gated by dependency promotion, and `archived` SHALL be restorable only to `triage`, `todo` or `ready`.
**Milestone:** M3

#### Scenario: Request changes lands in todo
- **GIVEN** a task in `review` whose parents are all `done`
- **WHEN** a reviewer requests changes
- **THEN** the task SHALL be in `todo` and then be promoted to `ready` by the next promotion pass

#### Scenario: Manual move outside the table
- **GIVEN** a task in `triage`
- **WHEN** a user drags it to `running` in the dashboard
- **THEN** the API SHALL return 409 with `illegal transition triage → running for user`

### Requirement: Task creation
`create` SHALL require a non-blank title (`title is required`), assign an id `t_` + 8 lowercase hex characters, validate `workspace` in `scratch` (default), `worktree[:<branch>]` or `dir:<absolute path>`, reject unknown parents (`unknown parent task(s): <ids>`) and unknown assignee profiles (`unknown profile '<p>'`), and set the initial status by an explicit optional `initial_status` of `triage`, `todo` or `blocked`, otherwise `todo` when any parent is not `done`/`archived` (recording a `dependency_wait` event) else `ready`.
**Milestone:** M3

#### Scenario: Child of an open parent waits
- **GIVEN** a parent task in `running`
- **WHEN** a child is created with that parent
- **THEN** the child SHALL be `todo` with a `dependency_wait` event naming the parent

#### Scenario: Relative dir workspace rejected at creation
- **GIVEN** `--workspace dir:../tenants/foo`
- **WHEN** the task is created
- **THEN** the system SHALL refuse with `workspace dir must be an absolute path` and create nothing

### Requirement: Idempotent create
When `idempotency_key` is given and a non-archived task with that key exists on the board, `create` SHALL return the existing task's id with `created: false` instead of inserting a duplicate.
**Milestone:** M3

#### Scenario: Retried automation does not duplicate
- **GIVEN** a task created with `--idempotency-key nightly-2026-10-29`
- **WHEN** the same create is repeated
- **THEN** the same id SHALL be returned and the board SHALL contain one such task

### Requirement: Dependency links
`link` SHALL reject self-links (`a task cannot depend on itself`), unknown ids, cycles (`linking <p> → <c> would create a cycle`) and links onto a `running` child unless the caller supplies that child's current run id; linking a `ready` child to an open parent SHALL demote it to `todo`, and an `archived` or `done` parent SHALL never gate. Links SHALL exist only within one board.
**Milestone:** M3

#### Scenario: Cycle refused
- **GIVEN** A depends on B
- **WHEN** B is linked to depend on A
- **THEN** the system SHALL refuse with `linking A → B would create a cycle` and change nothing

### Requirement: Ready promotion and atomic claim
The promotion pass SHALL move `todo` tasks whose parents are all `done`/`archived` to `ready`, and SHALL NOT promote a task blocked explicitly by a worker or human, a task created `blocked`, or a task whose breaker tripped; `claim` SHALL atomically move `ready → running` in one transaction, open a `task_runs` row with a fresh run id and a claim lease of `kanban.claim_ttl_secs` (default 900) renewed by worker heartbeats, and SHALL demote the task to `todo` with a `claim_rejected` event if a parent re-opened.
**Milestone:** M3

#### Scenario: Worker block is sticky
- **GIVEN** a task blocked by its worker whose parents are all done
- **WHEN** the promotion pass runs
- **THEN** the task SHALL stay `blocked`

### Requirement: Typed blocks and block-loop breaker
`block` SHALL take a kind in `dependency`, `needs_input`, `capability`, `budget` or `transient` and a reason; `dependency` SHALL route to `todo` (re-kinded `needs_input` when no parent is open); other kinds SHALL route to `blocked`, and a second block of the same kind after an unblock SHALL route the task to `triage` with a `block_loop_detected` event; `unblock` SHALL return the task to `review` if it was blocked out of review, else `ready` or `todo` by its parents, and only a successful completion SHALL clear the block history.
**Milestone:** M3

#### Scenario: Repeated same-cause block escalates
- **GIVEN** a task blocked `needs_input` and then unblocked
- **WHEN** it is blocked `needs_input` again
- **THEN** it SHALL be in `triage` with a `block_loop_detected` event

### Requirement: Scheduled parking
`schedule` SHALL move a `todo`, `ready`, `running` or `blocked` task to `scheduled` with an optional `wake_at` instant, ending any open run with outcome `scheduled`; a scheduled task SHALL never be claimed, and SHALL be re-gated automatically at `wake_at` (or by `unblock` when none is set).
**Milestone:** M3

#### Scenario: Wake at a time
- **GIVEN** a task scheduled with `wake_at` tomorrow 08:00 and all parents done
- **WHEN** that instant passes
- **THEN** the task SHALL become `ready` with a `woke` event

### Requirement: Completion gates
`complete` SHALL move a task to `done` only when all parents are `done`/`archived`, SHALL refuse with `completion blocked: <id> has no result or summary evidence` when the result and summary are blank (except a reviewer approving out of `review`), SHALL refuse when `created_tasks` names ids this run did not create, and SHALL refuse with `task <id> is running under a live worker; pass the run id or --force` when called without the matching run id while the run's worker is alive; each refusal SHALL append an auditable event.
**Milestone:** M3

#### Scenario: Whitespace evidence refused
- **GIVEN** a running task completed with result `"   "`
- **WHEN** `complete` is called
- **THEN** it SHALL be refused and the task SHALL keep its status

#### Scenario: Fabricated child cards refused
- **GIVEN** a worker that reports `created_tasks = ["t_deadbeef"]` it never created
- **WHEN** it completes
- **THEN** the completion SHALL be refused and a `hallucinated_cards` event SHALL be recorded

### Requirement: Review lane
When the task or board sets `review = true` (board default from `board.toml`, default false), a worker's successful completion SHALL move the task to `review` instead of `done`; `approve` SHALL move `review → done`, `request-changes` SHALL move it to `todo` with the reviewer's comment, and the dispatcher SHALL route `review` tasks to `reviewer` profiles when `kanban.review_dispatch = true`, reserving one concurrency slot for review work while a ready backlog exists.
**Milestone:** M3

#### Scenario: Agent reviewer
- **GIVEN** a board with `review = true` and a task whose `reviewer = "qa"`
- **WHEN** the worker completes it
- **THEN** the task SHALL be in `review` and the next tick SHALL spawn profile `qa` as the reviewer

### Requirement: Archive terminates the worker
`archive` SHALL set the task `archived` and, when it was `running`, SHALL terminate its worker after commit only if this caller won the transition, recording `archive_worker_termination` with the signal result; archived tasks SHALL drop their notification subscriptions.
**Milestone:** M3

#### Scenario: Archived running task leaves no worker
- **GIVEN** a running task with a live worker
- **WHEN** it is archived
- **THEN** the worker SHALL be terminated and the task SHALL be `archived`

### Requirement: Single dispatcher per home
The system SHALL run the dispatcher inside the gateway daemon (`kanban.dispatch = true`, default) every `kanban.dispatch_interval_secs` (default 30), and SHALL guarantee exactly one active dispatcher per Prometheus home through a lease row in each board's database (`holder`, `expires_at`, 60 s lease renewed every 20 s); `prometheus kanban dispatch --once` SHALL run a single tick and `--loop` a standalone loop, and either SHALL exit 75 with `dispatcher lease held by <holder>` when another live holder owns the lease.
**Milestone:** M3

#### Scenario: Standalone loop cannot race the daemon
- **GIVEN** a gateway daemon holding the dispatcher lease
- **WHEN** `prometheus kanban dispatch --loop` starts
- **THEN** it SHALL exit 75 without claiming any task

#### Scenario: Lease takeover after crash
- **GIVEN** a dispatcher that died holding the lease
- **WHEN** 60 s pass and another dispatcher ticks
- **THEN** the new dispatcher SHALL take the lease and record a `dispatcher_takeover` event

### Requirement: Dispatcher tick
A tick SHALL, per board, reclaim expired and orphaned runs, run promotion and scheduled wake-ups, then claim and start spawnable `ready` tasks (and `review` tasks when enabled) in priority then age order; an unassigned ready task SHALL take `kanban.default_assignee` when set and otherwise be reported as `skipped_unassigned`; each tick SHALL emit a summary event (`claimed`, `skipped_*`, `reclaimed`) visible in `prometheus kanban diag`.
**Milestone:** M3

#### Scenario: Unassigned task is reported
- **GIVEN** a ready task with no assignee and no default assignee
- **WHEN** the dispatcher ticks
- **THEN** the task SHALL stay `ready` and the tick summary SHALL list it under `skipped_unassigned`

### Requirement: Concurrency caps
`kanban.max_running` SHALL cap running workers across all boards in the home (default: physical memory / 512 MiB clamped to [2, 8], or 2 when memory is unknown), and `kanban.max_running_per_profile` (default 2) SHALL cap concurrent workers per assignee; deferred tasks SHALL be reported as `capped`.
**Milestone:** M3

#### Scenario: Host cap counts other boards
- **GIVEN** `kanban.max_running = 2` and two tasks running on another board
- **WHEN** this board is ticked
- **THEN** no worker SHALL be started

### Requirement: Claim allowlist fails closed
When `kanban.dispatch_profiles` is set, the dispatcher SHALL claim only tasks assigned to those profiles; an empty list SHALL claim nothing; unset SHALL allow any existing profile; a task assigned to a profile that does not exist SHALL be blocked with kind `capability` and reason `assignee profile '<p>' does not exist`.
**Milestone:** M3

#### Scenario: Deleted assignee
- **GIVEN** a ready task assigned to profile `ghost` that does not exist
- **WHEN** the dispatcher ticks
- **THEN** the task SHALL be `blocked` with kind `capability`

### Requirement: Failure circuit breaker
Every non-success run outcome (spawn failure, crash, non-zero exit, timeout, expired claim) SHALL increment the task's `consecutive_failures` and store `last_failure` (truncated to 500 characters); when failures reach the task's `max_retries`, else `kanban.failure_limit` (default 2), the task SHALL be blocked with a `gave_up` event. A worker exit code 78 (credential revoked, model gone, capability denied) SHALL trip the breaker on the first attempt; exit code 75 (rate limited) SHALL requeue without counting and with a cooldown of `kanban.rate_limit_cooldown_secs` (default 300); a spawn the dispatcher itself refused (cap, lease loss) SHALL NOT count; a successful completion SHALL reset the counter.
**Milestone:** M3

#### Scenario: Terminal exit blocks immediately
- **GIVEN** a worker that exits 78
- **WHEN** the dispatcher reaps it
- **THEN** the task SHALL be `blocked` with a `gave_up` event after one attempt

#### Scenario: Rate-limit exit does not count
- **GIVEN** a worker that exits 75
- **WHEN** the dispatcher reaps it
- **THEN** the task SHALL be `ready` with `not_before` 300 s ahead and `consecutive_failures` unchanged

### Requirement: Worker spawn by run ticket
The dispatcher SHALL start a worker as `prometheus kanban work --run <run-id>` under the assignee's ProfileScope, passing a single-use run ticket on stdin and a scrubbed environment containing only `PATH`, locale variables and `PROMETHEUS_HOME`; the worker SHALL resolve its board, task, workspace and claim from the ticket, load secrets only from the assignee profile's own secrets store, and SHALL refuse to start (exit 78) when the ticket is missing, reused or does not match an open run.
**Milestone:** M3

#### Scenario: Launch profile secrets do not leak
- **GIVEN** a daemon launched as profile `ops` dispatching a task assigned to `writer`
- **WHEN** the worker starts
- **THEN** none of `ops`'s secrets SHALL be present in the worker's environment or secrets handle

#### Scenario: Replayed ticket refused
- **GIVEN** a run ticket already consumed by a worker
- **WHEN** a second process presents it
- **THEN** that process SHALL exit 78 with `run ticket already used`

### Requirement: Worker liveness and heartbeats
Workers SHALL heartbeat every 60 s (renewing the claim lease), and the dispatcher SHALL treat a worker as alive only when its PID and process start time both match the values recorded at spawn; a recycled PID SHALL be reclaimed without being signalled, a run whose lease expired SHALL be reclaimed as a failure, and a worker still alive 120 s after its run closed SHALL be terminated with a `worker_reaped` event.
**Milestone:** M3

#### Scenario: Recycled PID is not killed
- **GIVEN** a running task whose recorded PID now belongs to another process
- **WHEN** reclaim runs
- **THEN** the task SHALL be reclaimed and that PID SHALL NOT be signalled

### Requirement: Kanban toolset and grants
The `kanban` toolset SHALL provide `kanban_show`, `kanban_list`, `kanban_create`, `kanban_comment`, `kanban_attach`, `kanban_link`, `kanban_complete`, `kanban_block`, `kanban_schedule`, `kanban_heartbeat`, `kanban_request_review`, `kanban_request_changes`, `kanban_unblock`, `kanban_attach_url` and `kanban_attachments`; a worker session SHALL receive it automatically with a capability grant scoped to its own board (without `kanban_list` and `kanban_unblock`), other sessions SHALL receive it only when `kanban` is explicitly listed in their toolsets, and delegated children SHALL receive no kanban grant; run-lifecycle tools SHALL refuse outside a worker session with `not a kanban worker session`.
**Milestone:** M3

#### Scenario: Worker cannot touch another board
- **GIVEN** a worker for a task on board `alpha`
- **WHEN** it calls `kanban_create` with `board = "beta"`
- **THEN** the call SHALL be refused with `capability denied: kanban board 'beta'`

#### Scenario: Delegated child has no board access
- **GIVEN** a subagent delegated by a worker
- **WHEN** its tool list is built
- **THEN** it SHALL contain no `kanban_*` tool

### Requirement: Workspaces
A `scratch` workspace SHALL be created under the board's `workspaces/<task_id>/` and deleted after completion once declared artifacts were copied into the task's attachments (a missing declared artifact SHALL refuse completion); `worktree` SHALL be a git worktree on a deterministic branch `kanban/<task_id>` (or the given branch; on a board bound to a project the branch `<project-slug>/<task-id>[-<title-slug>]` of `projects` › Kanban board binding instead) and be preserved; `dir:<path>` SHALL be preserved; and every worker's local sandbox SHALL grant filesystem write access to its workspace only.
**Milestone:** M3

#### Scenario: Missing artifact keeps task in flight
- **GIVEN** a task declaring artifact `report.pdf` that the worker did not produce
- **WHEN** the worker completes
- **THEN** the completion SHALL be refused with `declared artifact missing: report.pdf`

### Requirement: Attachments
Attachments SHALL be stored in the blob store and indexed per task with sanitized display names, and SHALL be refused above `kanban.attachment_max_bytes` (default 25 MiB) on every surface with `attachment too large: <size> > <limit>`.
**Milestone:** M3

#### Scenario: Oversized upload refused
- **GIVEN** a 26 MiB file
- **WHEN** it is attached from the CLI, the dashboard or a tool
- **THEN** each surface SHALL refuse it and store nothing

### Requirement: Notifications through the owning profile
Notification subscriptions SHALL record adapter, chat, thread and subscriber profile, and events `completed`, `blocked`, `gave_up` and `review_requested` SHALL be delivered through the subscriber profile's adapter under its ProfileScope; a route that profile cannot reach SHALL be skipped with one WARNING per subscription naming the remedy, never sent through another profile's adapter; subscriptions on done tasks SHALL be purged after `kanban.done_sub_retention_secs` (default 2592000).
**Milestone:** M3

#### Scenario: Multi-profile notification
- **GIVEN** a task subscribed by profile B on a daemon also serving profile A
- **WHEN** the task completes
- **THEN** the notification SHALL be sent with B's adapter credentials

### Requirement: Per-task cost budget (upgrade)
The system SHALL accept optional per-task `budget_usd` and `budget_tokens` limits; the worker session SHALL enforce them through the usage ledger including delegated children, and on exhaustion the worker SHALL stop the turn, record the spend and block the task with kind `budget`; `prometheus kanban show` SHALL display cumulative spend across all runs of the task.
**Milestone:** M3

#### Scenario: Budget exhausted
- **GIVEN** a task with `budget_usd = 0.50`
- **WHEN** its worker's cumulative spend reaches 0.50 USD
- **THEN** the run SHALL stop and the task SHALL be `blocked` with kind `budget` and the spend recorded

### Requirement: Eval and harness-evolution tasks (upgrade)
A task SHALL have `kind` `agent` (default), `eval` or `evolve`: an `eval` task (fields `suite`, optional `provider_mode = live|replay`) SHALL run `prometheus eval run` for the assignee profile and attach the report, completing with the suite's summary as result; an `evolve` task (fields `target_task`, `mode = parallel|sequential`, `budget_k`) SHALL run a harness-evolution session under the assignee's evolution budget and SHALL always finish in `review`, never `done`, so that adopting the evolved harness requires a human approval, which applies it from the profile's next session.
**Milestone:** M3

#### Scenario: Evolve task requires approval
- **GIVEN** an `evolve` task for profile `coder` with `budget_k = 2`
- **WHEN** the evolution run finishes
- **THEN** the task SHALL be in `review` with the selected harness diff attached and the profile's active harness SHALL be unchanged until `approve`

#### Scenario: Eval task on replayed provider
- **GIVEN** an `eval` task with `suite = "smoke"` and `provider_mode = "replay"`
- **WHEN** it runs
- **THEN** no live provider request SHALL be made and the eval report SHALL be attached

### Requirement: Event log and run history
Every task mutation SHALL append an immutable event (`id`, `task_id`, `run_id`, `actor`, `kind`, `payload`, `at`) and every attempt SHALL be a `task_runs` row with outcome, exit code, session id, trajectory id and spend; `prometheus kanban log <task>` SHALL print events and `prometheus kanban runs <task>` SHALL print attempts, and a worker's session SHALL be openable from the run record.
**Milestone:** M3

#### Scenario: Audit a refusal
- **GIVEN** a refused completion
- **WHEN** `prometheus kanban log <task>` runs
- **THEN** the refusal event with its reason SHALL be listed

### Requirement: CLI surface
`prometheus kanban` SHALL provide `create`, `list` (alias `ls`), `show`, `edit`, `assign`, `link`, `unlink`, `comment`, `attach`, `attachments`, `block`, `unblock`, `schedule`, `complete`, `approve`, `request-changes`, `archive`, `log`, `runs`, `tail`, `stats`, `diag`, `dispatch`, `work`, `notify subscribe|list|unsubscribe`, `gc`, the verbs listed in Additional CLI verbs and `boards list|create|rename|archive|switch|export|import`; lifecycle verbs SHALL accept multiple task ids and report per-id results, exiting 1 if any id failed.
**Milestone:** M3

#### Scenario: Bulk archive with one failure
- **GIVEN** ids `t_a`, `t_b` and an unknown `t_zz`
- **WHEN** `prometheus kanban archive t_a t_b t_zz` runs
- **THEN** `t_a` and `t_b` SHALL be archived, `t_zz` SHALL be reported `not found`, and the command SHALL exit 1

### Requirement: Board export and import
`prometheus kanban boards export <slug> <file>` SHALL write a versioned archive of tasks, links, events, runs and attachments, and `import` SHALL restore it as a new board, refusing an existing slug unless `--rename <new>` is given and remapping task ids only when they collide.
**Milestone:** M3

#### Scenario: Round trip
- **GIVEN** a board exported to a file
- **WHEN** it is imported as `copy`
- **THEN** `copy` SHALL contain the same tasks, links and attachments

### Requirement: URL attachments and attachment listing tools
The `kanban` toolset SHALL also provide `kanban_attach_url` (`task_id` defaulting to the worker's own task, required `url`, optional `filename` defaulting to the URL path's last segment, optional `content_type` defaulting to the response's `Content-Type`) and `kanban_attachments` (`task_id` defaulting to the worker's own task) returning rows `{id, filename, content_type, size, uploader, path}` where `path` is a read-only local path the session's file tools may read; `kanban_attach_url` SHALL accept only `http` and `https` URLs, fetch server-side through the egress proxy (refusing private, loopback and link-local destinations as `security-boundaries` defines), stream with a cap of `kanban.attachment_max_bytes` (aborting and storing nothing above it), and in a worker session both tools SHALL be scoped by the worker's board grant, with attaching allowed only to the worker's own task.
**Milestone:** M3

#### Scenario: Worker attaches a report by link
- **GIVEN** a worker for task `t_1a2b3c4d`
- **WHEN** it calls `kanban_attach_url` with `url = "https://example.com/r/report.pdf"`
- **THEN** `kanban_attachments` SHALL list `report.pdf` with its size and uploader set to the worker's profile

#### Scenario: Internal address refused
- **WHEN** `kanban_attach_url` is called with `url = "http://169.254.169.254/latest/meta-data"`
- **THEN** the call SHALL be refused by the egress policy and nothing SHALL be stored

### Requirement: PR completion contracts
`create` (CLI `--completion-contract`, tool field `completion_contract`) SHALL accept `OWNER/REPO`, an exact pull-request URL `https://github.com/OWNER/REPO/pull/<n>`, or `local-only` (the default for tasks that declare none), persisted and shown by `show --json` and `kanban_show`; for a repository contract, every completion path (worker tools, CLI, review approval, dashboard) SHALL require `metadata.published_pr` naming a PR in that repository, SHALL bind the first matching PR URL permanently (a later different URL refused with `completion contract already bound to <url>`), and SHALL complete only when every required status check and ruleset context on the PR's current head commit has passed, reading branch protection, rulesets, check runs and legacy statuses with the assignee profile's own GitHub credential from its secrets store (never an ambient login; a missing credential refused with `not logged in for profile <p>`); missing, pending, failed, cancelled, timed-out, stale, skipped or neutral required evidence, a repository with no required checks, or any API failure SHALL refuse completion, keep the task and workspace, and append a `pr_acceptance` event recording PR URL, head SHA, required contexts, check ids, classification (`checks`, `auth`, `infra`, `policy`) and the next step, with no remote writes.
**Milestone:** M3

#### Scenario: Red required check blocks completion
- **GIVEN** a task with `completion_contract = "acme/api"` and a PR whose required check `ci/test` failed
- **WHEN** the worker calls `kanban_complete` with `metadata.published_pr` set to that PR
- **THEN** the completion SHALL be refused, the task SHALL stay `running`, and a `pr_acceptance` event SHALL name `ci/test` with classification `checks`

#### Scenario: Retry cannot substitute a sibling PR
- **GIVEN** a contract already bound to PR 41
- **WHEN** completion is retried with `metadata.published_pr` naming PR 42 whose checks are green
- **THEN** the completion SHALL be refused with `completion contract already bound to`

### Requirement: Goal-mode cards
A task created with `goal_mode = true` (CLI `--goal`, optional `--goal-max-turns N`, default 20) SHALL run its worker in the goal loop engine defined by `session-automation`, using the task title and body as acceptance criteria: after each worker turn the auxiliary judge SHALL decide `done`, `continue` or `unachievable`; `continue` SHALL run another turn in the same session while turns remain; exhausting the turn budget SHALL block the task with kind `needs_input` and reason `goal budget exhausted`; `unachievable` SHALL block it immediately with the judge's reason; `complete` and `request-review` on a goal-mode task SHALL be refused only on a real `continue` or `unachievable` verdict, while a judge call that fails SHALL allow the handoff with a WARN `goal judge unreachable; allowing lifecycle handoff`; each judged turn SHALL append a log line `kanban goal loop: turn <n>/<m> verdict=<v>`.
**Milestone:** M3

#### Scenario: Judge keeps the worker going
- **GIVEN** a goal-mode task `translate every page` whose worker stops after translating half
- **WHEN** the judge returns `continue`
- **THEN** the worker SHALL take another turn in the same session and the log SHALL contain `turn 2/20 verdict=continue`

#### Scenario: Impossible card is never done
- **WHEN** the judge returns `unachievable` with reason `source repo does not exist`
- **THEN** the task SHALL be `blocked` with that reason and `kanban complete` on it SHALL be refused

### Requirement: Per-task model override and pinned skills
A task SHALL accept optional `model` and `provider` (CLI `--model`/`--provider`; `--provider` requires `--model`) and a `skills` list (CLI repeatable `--skill`, tool field `skills`), validated at create against the provider registry and the assignee profile's installed skills (`unknown skill '<s>' for profile '<p>'`); the dispatcher SHALL start the worker session with that model instead of the assignee's default and with those skills preloaded on top of the kanban worker guidance; `prometheus kanban set-model <id> <model>|none [--provider <p>]` SHALL change or clear the override, taking effect at the next dispatch; a pinned skill missing at spawn time SHALL block the task with kind `capability`.
**Milestone:** M3

#### Scenario: Hard card on a stronger model
- **GIVEN** assignee `coder` configured with a small model and a task created with `--model big-model --provider anthropic`
- **WHEN** the dispatcher spawns the worker
- **THEN** the worker session SHALL use `big-model` via `anthropic`

### Requirement: Respawn guard and dispatch dry run
The dispatcher SHALL NOT spawn a worker for a `ready` task when its previous run ended on a quota, authentication or HTTP 429 error (`blocker_auth`), when it completed a run successfully within the last `kanban.respawn_success_window_secs` (default 3600, `recent_success`), or when a comment within the last `kanban.respawn_pr_window_secs` (default 86400) links a GitHub pull request (`active_pr`), recording a `respawn_guarded` event with the reason and never counting it as a failure; `recent_success` SHALL be lifted by a deliberate re-queue (drag `done → ready`, `unblock`, re-promotion), `active_pr` SHALL be lifted for a different profile named by a later reassign, request-changes or review reopen, and neither guard SHALL apply to the review lane; `prometheus kanban dispatch --dry-run [--json]` SHALL change nothing and print `Guarded (<reason>): <id>` per held task plus rate-limited, capped and locked tasks.
**Milestone:** M3

#### Scenario: Fresh PR is not re-worked
- **GIVEN** a ready task whose worker commented a PR URL 2 hours ago
- **WHEN** the dispatcher ticks
- **THEN** no worker SHALL be spawned and `prometheus kanban dispatch --dry-run` SHALL print `Guarded (active_pr): <id>`

### Requirement: Swarm topology helper
`prometheus kanban swarm "<goal>" --workers <p1,p2,...> --verifier <p> --synthesizer <p> [--board B]` SHALL create in one transaction a root card in `done` holding the goal and a structured JSON blackboard as comments, one worker card per listed profile linked under the root, a verifier card gated on all worker cards and a synthesizer card gated on the verifier, so readers see either no swarm or the complete graph; unknown profiles SHALL refuse the whole swarm, and workers SHALL be able to read and append blackboard entries with `kanban_show` and `kanban_comment` on the root.
**Milestone:** M3

#### Scenario: Atomic swarm creation
- **WHEN** `prometheus kanban swarm "failover plan" --workers researcher,architect,sre --verifier reviewer --synthesizer writer` runs
- **THEN** exactly six cards SHALL be created with the verifier `todo` until all three worker cards are `done`

#### Scenario: Unknown worker profile aborts everything
- **WHEN** the same command names a nonexistent profile `ghost`
- **THEN** it SHALL exit 66 with `unknown profile 'ghost'` and no card SHALL be created

### Requirement: /kanban slash command
Every `prometheus kanban` verb SHALL be available as `/kanban <verb> ...` on every chat surface (CLI, TUI, desktop, gateway) through the same parser, flags and output, registered with `busy_policy = immediate` so it runs mid-turn without interrupting or queueing behind the agent; when `/kanban create` runs from a gateway chat without `--json`, the originating chat (adapter, chat, thread, chat type, subscriber profile) SHALL be subscribed to the new task in `notify+wake` mode and the reply SHALL say so; output above 3800 characters on messaging adapters SHALL be truncated with `… (truncated; run prometheus kanban <verb> in a terminal for full output)`.
**Milestone:** M3

#### Scenario: Unblock from a phone mid-turn
- **GIVEN** a Telegram chat whose agent is mid-turn
- **WHEN** the user sends `/kanban unblock t_abcd1234`
- **THEN** the task SHALL be unblocked immediately and the running turn SHALL continue undisturbed

#### Scenario: Created card reports back
- **WHEN** the user sends `/kanban create "transcribe podcast" --assignee transcriber` in Slack
- **THEN** the chat SHALL be subscribed and SHALL receive a message when the task completes or blocks

### Requirement: Subscription delivery modes and inheritance
A notification subscription SHALL carry `delivery_mode` `notify` (default: passive message only), `notify+wake` (passive message and a turn for the subscriber profile's agent in that chat's session) or `wake` (turn only), and `chat_type` (`dm`, `group`, `channel`, `thread`) so the wake resolves the same session key the chat uses; wake turns SHALL fire only for `completed`, `blocked`, `gave_up`, `crashed`, `timed_out`, `review_requested` and `block_loop_detected` events (status, archived and unblocked events never wake), SHALL carry the event's summary, SHALL run under the subscriber profile's ProfileScope and adapter, and a wake that cannot be admitted SHALL be retried on later notifier ticks without re-sending an already checkpointed passive message; a task created by a worker (tool or CLI) SHALL copy its owning task's subscriptions with their modes unchanged; `prometheus kanban notify subscribe` SHALL accept `--delivery-mode` and `--chat-type`.
**Milestone:** M3

#### Scenario: Review feedback wakes the originating chat
- **GIVEN** a `notify+wake` subscription from a Discord thread on task `t_1`
- **WHEN** a reviewer requests changes and the worker later raises `review_requested`
- **THEN** the thread SHALL receive the passive message and the subscriber profile's agent SHALL take one turn in that thread's session with the summary in context

### Requirement: Orchestrator guidance, specify and decompose
A worker session SHALL receive in its frozen system prompt the kanban worker guidance and, when the task's assignee profile has `kanban.role = "orchestrator"`, orchestrator guidance instructing it to discover existing profiles first, decompose the goal into linked tasks assigned to existing profiles with every shared decision written into each child body, and not do the implementation itself; `prometheus kanban specify <id>|--all [--tenant T]` SHALL use the auxiliary model (relay-affinity key `kanban:<task-id>`) to expand a `triage` task's one-line idea into a full body with acceptance criteria and move it to `todo`, and `prometheus kanban decompose <id>` SHALL produce a validated child-task graph (assignees must exist, no cycles) created in one transaction under the task, promoting children with no open parents to `ready` when `kanban.auto_promote_children = true` (default) and leaving them `todo` otherwise; `kanban.auto_decompose` (default `false`, the dashboard's Orchestration Auto/Manual control) SHALL, when true, make the dispatcher run `specify` on a `triage` task and `decompose` on a task assigned to an orchestrator profile before dispatching it, and when false leave both to explicit commands.
**Milestone:** M3

#### Scenario: Specify promotes a triage idea
- **GIVEN** a `triage` task titled `add rate limiting`
- **WHEN** `prometheus kanban specify <id>` runs
- **THEN** the task body SHALL contain acceptance criteria and its status SHALL be `todo`

#### Scenario: Decompose rejects unknown assignees
- **GIVEN** the auxiliary model proposes a child assigned to profile `nobody`
- **WHEN** `prometheus kanban decompose <id>` validates the graph
- **THEN** no child SHALL be created and the command SHALL exit 65 naming `nobody`

### Requirement: Additional CLI verbs
`prometheus kanban` SHALL additionally provide `init` (create the default board and print the dispatcher status), `claim <id> [--ttl <secs>]` (manual claim for an external worker, returning the run id and ticket), `context <id>` (print exactly the context a worker would receive), `request-review <id> [--summary S] [--metadata JSON] [--reviewer P]` (accepted from `running` or `ready`), `reopen-review <id>... [--reason R]` (move `review` back to `ready`/`todo`), `set-model`, `swarm`, `specify`, `decompose`, `promote <id>...`, `reassign <id>... <profile>`, `heartbeat <id> [--note N]`, `watch [--assignee P] [--kinds K]` and `assignees [--json]`, each routed through the typed workflow table and recorded in the event log.
**Milestone:** M3

#### Scenario: Inspect worker context
- **WHEN** `prometheus kanban context t_abcd1234` runs
- **THEN** it SHALL print the task, parent results, comments and attachments exactly as the worker's first message would contain them, without claiming the task

### Requirement: Worker lanes
The dispatcher SHALL resolve a task's assignee to a worker lane: a Prometheus profile (spawned as defined by Worker spawn by run ticket) or an external lane declared by an enabled plugin in `plugin.toml` under `[[kanban_lanes]]` with `name`, a `command` run under the local sandbox in the task's workspace with the run ticket on stdin, and the capability grants the plugin was approved for; an external lane's process SHALL report only through the `prometheus kanban` CLI or the kanban RPC methods authenticated by its run ticket, and every claim SHALL end in exactly one terminator (`complete`, `request-review`, `block`) or be reclaimed as a failure by the liveness rules; an assignee matching neither SHALL leave the task `ready` with a `skipped_nonspawnable` event, and a lane name colliding with a profile name SHALL fail plugin load.
**Milestone:** M3

#### Scenario: External CLI lane completes a task
- **GIVEN** an enabled plugin declaring lane `codex-cli` and a ready task assigned to `codex-cli`
- **WHEN** the dispatcher ticks
- **THEN** the lane command SHALL start in the task workspace with a run ticket and its `prometheus kanban complete` call SHALL move the task to `done`

#### Scenario: Unknown assignee is surfaced
- **GIVEN** a ready task assigned to `nonexistent-lane`
- **WHEN** the dispatcher ticks
- **THEN** the task SHALL stay `ready` with a `skipped_nonspawnable` event

# Session Automation

## Purpose
Session automation keeps one session working without the user re-prompting: a standing `/goal` driven by an auxiliary judge (with optional completion contracts, subgoals, deterministic quality gates and wait barriers), a timer-driven `/loop`, and an idle-only `/heartbeat`. The key decision is that every automated wakeup is an ordinary user-role turn injected only at an idle turn boundary, so it never mutates the system prompt, tools or history and never breaks the prompt cache; all automation state is persisted per session in `state.db`, survives resume and compaction, and is arbitrated by one scheduler in which a real user message always wins.

## ADDED Requirements

### Requirement: Persisted per-session automation state
The system SHALL hold at most one goal, one loop and one heartbeat per session, SHALL persist each in the profile's `state.db` keyed by session id (`automation` table, columns `session_id`, `kind` in `goal|loop|heartbeat`, `state` JSON, `owner_surface`, `updated_at`), SHALL carry the state across `/resume`, daemon restart and in-place context compaction unchanged, and SHALL drop all three when the session is reset, deleted or switched away from on a messaging conversation (a later resume of that archived conversation SHALL NOT resurrect them).
**Milestone:** M2

#### Scenario: Goal survives resume
- **GIVEN** a session with an active goal that has used 4 of 20 turns
- **WHEN** the user closes the CLI and later runs `prometheus chat --resume <id>`
- **THEN** `/goal status` SHALL report the same goal text, status `active` and `4/20` turns used

#### Scenario: Reset clears automation
- **GIVEN** a Telegram conversation with an active heartbeat
- **WHEN** the user runs `/new` in that chat
- **THEN** the heartbeat row SHALL be removed and SHALL NOT fire for the archived session

### Requirement: Automated turns are plain user turns
Every goal continuation, loop wakeup and heartbeat fire SHALL be appended as a user-role message whose stored metadata carries `origin` (`goal` | `loop` | `heartbeat`) and SHALL enter the normal turn entry contract of the agent turn loop; it SHALL NOT modify the system prompt, toolset, memory snapshot or earlier messages, so the request prefix of the previous turn SHALL remain a byte-identical prefix of the automated turn's request.
**Milestone:** M2

#### Scenario: Cache prefix preserved
- **GIVEN** a goal driving a third continuation turn
- **WHEN** the continuation request is built
- **THEN** its canonical prefix hash up to the previous turn SHALL equal the previous request's hash and no `cache.unexpected_break` SHALL be emitted

### Requirement: Idle-boundary arbitration
The system SHALL fire automation only when the session is idle (no active turn and no queued user input), SHALL give precedence in the order queued user message, then active goal continuation, then loop wakeup, then heartbeat, SHALL defer loop and heartbeat wakeups while a goal is `active` and not parked, and SHALL let them use idle time while the goal is paused, parked on a wait barrier, done or cleared; at most one automated turn SHALL be admitted per idle boundary.
**Milestone:** M2

#### Scenario: User input preempts continuation
- **GIVEN** an active goal whose judge returned `continue` and a user message queued during the turn
- **WHEN** the turn ends
- **THEN** the user's message SHALL run next and the judge SHALL evaluate the goal again after that turn

#### Scenario: Parked goal lets the loop run
- **GIVEN** a goal parked on a background pid and an active `/loop 2m`
- **WHEN** two minutes pass while the session is idle
- **THEN** the loop wakeup SHALL fire while the goal remains parked

### Requirement: Goal command surface
Every surface (CLI, TUI, desktop, dashboard chat, messaging gateway) SHALL parse `/goal` through one shared handler supporting `/goal <text>` (set or replace, and start the first turn immediately), `/goal -- <text>` (literal text starting with a control word), `/goal draft <text>`, `/goal show`, `/goal` and `/goal status`, `/goal pause`, `/goal resume`, `/goal clear` (aliases `stop`, `done`), `/goal wait <pid> [reason]`, `/goal unwait` and `/goal gate add|list|remove <N>|clear`; a control word followed by more words SHALL run the control command and ignore the rest, and `draft` SHALL be matched only as a whole word.
**Milestone:** M2

#### Scenario: Control word escape
- **GIVEN** no active goal
- **WHEN** the user runs `/goal -- pause the nightly cron`
- **THEN** the goal text SHALL be `pause the nightly cron` and no pause command SHALL run

#### Scenario: Draft is a whole word
- **WHEN** the user runs `/goal drafting docs`
- **THEN** the goal text SHALL be `drafting docs` and no draft model call SHALL be made

### Requirement: Goal changes while a turn runs
While a turn is running, `/goal status`, `pause`, `clear`, `wait`, `unwait` and `gate list|remove|clear` SHALL only update control-plane state without interrupting the turn, and setting a new goal SHALL be rejected with `A turn is running; use /stop first, then set the new goal.` so an old continuation cannot race a new one.
**Milestone:** M2

#### Scenario: New goal rejected mid-run
- **GIVEN** a gateway session with a running turn
- **WHEN** the user sends `/goal write the report`
- **THEN** the system SHALL reply with the `/stop first` message and the existing goal SHALL be unchanged

### Requirement: Goal judge loop
After each turn of a session with an `active` goal the system SHALL call the auxiliary task `goal_judge` with the goal text, its contract and subgoals, the last 4000 characters of the final assistant response and the session's live background processes, SHALL require a one-line JSON verdict `{"verdict": "done"|"continue"|"blocked"|"wait", "reason": "...", "wait_on_session"?, "wait_on_pid"?, "wait_for_seconds"?}` (also accepting the legacy `{"done": bool, "reason": "..."}`), SHALL inject a continuation turn on `continue`, mark the goal `done` on `done`, and SHALL treat a judge error, timeout or malformed reply as `continue` (fail-open) with the error logged at WARN.
**Milestone:** M2

#### Scenario: Continue injects the next turn
- **GIVEN** an active goal and a judge reply `{"verdict":"continue","reason":"3 files remain"}`
- **WHEN** the turn ends
- **THEN** the surface SHALL show `Continuing toward goal (1/20): 3 files remain` and a continuation turn SHALL run

#### Scenario: Broken judge fails open
- **GIVEN** the `goal_judge` route returns HTTP 500
- **WHEN** the turn ends
- **THEN** the verdict SHALL be treated as `continue` and the turn counter SHALL advance

### Requirement: Goal turn budget and blocked verdict
The system SHALL count continuation turns against `goals.max_turns` (default 20, range 1..1000) and SHALL auto-pause the goal when the count reaches it with `Goal paused — 20/20 turns used. Use /goal resume to keep going, or /goal clear to stop.`; a `blocked` verdict SHALL pause the goal with the judge's reason and SHALL never mark it done; `/goal resume` SHALL reset the counter to zero and setting a new goal SHALL clear subgoals, gates and barriers.
**Milestone:** M2

#### Scenario: Budget exhaustion pauses
- **GIVEN** `goals.max_turns = 3` and a judge that always says `continue`
- **WHEN** the third continuation turn ends
- **THEN** the goal status SHALL be `paused` and no fourth continuation SHALL run

#### Scenario: Unachievable goal pauses
- **WHEN** the judge returns `{"verdict":"blocked","reason":"requires production DB credentials"}`
- **THEN** the goal SHALL be `paused` with that reason and SHALL NOT be reported as achieved

### Requirement: Completion contracts
A goal SHALL optionally carry a completion contract with the fields `outcome`, `verification`, `constraints`, `boundaries` and `stop_when`; inline lines whose prefix is one of `verify:`, `verified by:`, `constraints:`, `preserve:`, `boundaries:`, `scope:`, `stop when:` or `blocked:` SHALL populate the matching field while every other line (including lines with incidental colons) stays in the goal headline; `/goal draft <text>` SHALL ask `goal_judge` to expand the objective into a contract, set it and print it, falling back to a free-form goal when drafting fails; with a contract set the judge SHALL return `done` only with concrete evidence for `verification`, and `/goal show` SHALL print the contract.
**Milestone:** M2

#### Scenario: Inline contract parsing
- **WHEN** the user sets a goal `Fix bug: parser drops commas` followed by a line `verify: cargo test -p parser passes`
- **THEN** the headline SHALL be `Fix bug: parser drops commas` and `verification` SHALL be `cargo test -p parser passes`

#### Scenario: Draft falls back
- **GIVEN** the `goal_judge` route is unavailable
- **WHEN** the user runs `/goal draft migrate auth to JWT`
- **THEN** the goal SHALL be set with headline `migrate auth to JWT`, no contract, and the first turn SHALL start

### Requirement: Subgoals
The system SHALL provide `/subgoal <text>` (append a numbered criterion to the active goal; refused with `No active goal.` otherwise), `/subgoal` (list), `/subgoal remove <N>` (1-based) and `/subgoal clear`, without resetting the turn counter; the next continuation prompt SHALL list the subgoals under `Additional criteria the user added mid-loop`, and the judge SHALL return `done` only when the goal and every subgoal are met.
**Milestone:** M2

#### Scenario: Subgoal tightens the judge
- **GIVEN** an active goal `fix the failing tests` and `/subgoal add a regression test`
- **WHEN** the judge evaluates a response that fixed the tests but added no regression test
- **THEN** the judge prompt SHALL include the subgoal and a `done` verdict SHALL NOT be accepted without evidence for it

### Requirement: Quality gates
The system SHALL let a goal carry shell-command quality gates (`/goal gate add <command>`), SHALL run every gate at each turn boundary before the judge inside the session's sandbox and working directory with a timeout of `goals.gate_timeout_secs` (default 300), SHALL skip the judge when any gate exits non-zero and use the exit code plus the last 3072 bytes of combined output as the continuation prompt, SHALL re-run a failed gate on every boundary (never replaying a stale result), and SHALL auto-pause the goal when a gate has failed `goals.gate_max_retries` (default 3) consecutive times; on messaging surfaces `gate add` SHALL require a configured gateway admin while list, remove and clear stay available to the session's users.
**Milestone:** M2

#### Scenario: Red gate blocks completion
- **GIVEN** a goal with gate `cargo test` that exits 101
- **WHEN** the turn ends
- **THEN** the judge SHALL NOT be called and the next continuation SHALL contain `exit 101` and the output tail

#### Scenario: Exhausted gate pauses
- **GIVEN** the same gate failing on three consecutive boundaries
- **WHEN** the third failure is recorded
- **THEN** the goal SHALL be `paused` with a message naming the gate and suggesting `/goal gate remove 1` or `/goal resume`

### Requirement: Goal wait barriers
The system SHALL park an active goal on a judge `wait` verdict naming `wait_on_session <process session id>` (released when that background process exits or its watch pattern matches), `wait_on_pid <pid>` (released on exit) or `wait_for_seconds <n>`, and on `/goal wait <pid> [reason]`; while parked no judge call, continuation or turn count SHALL occur; a pid or session wait SHALL be capped at 30 minutes; a pid that is not a live local process SHALL be rejected for `/goal wait` and turn a judge `wait_on_pid` into `continue`; only processes started by the session itself (not by its subagents) SHALL be shown to the judge; the barrier SHALL be persisted with the goal and dropped by `/goal unwait`, `pause`, `resume` and `clear`.
**Milestone:** M2

#### Scenario: Park on a CI watcher
- **GIVEN** the agent started a background process with `notify_on_complete = true` and the judge returns `{"verdict":"wait","wait_on_pid":4242,"reason":"CI running"}`
- **WHEN** later idle boundaries occur while pid 4242 runs
- **THEN** no continuation SHALL be injected and `/goal status` SHALL show the goal as parked on pid 4242

#### Scenario: Dead pid rejected
- **WHEN** the user runs `/goal wait 99999` for a pid that does not exist
- **THEN** the system SHALL refuse with `process 99999 is not running` and the goal SHALL stay active

### Requirement: Goal messages on messaging platforms
On messaging platforms a goal continuation, loop wakeup and heartbeat output SHALL be posted to the originating chat or thread without quoting the message that set the automation, while replies to real user messages keep the platform's normal reply behavior; the status lines `Goal set`, `Continuing toward goal`, `Goal achieved`, `Goal paused` and `Goal judged unachievable — paused` SHALL be delivered on every surface.
**Milestone:** M2

#### Scenario: Telegram continuation not quoted
- **GIVEN** a Telegram chat where the goal was set by message 100
- **WHEN** the second continuation produces a reply
- **THEN** the outbound message SHALL NOT set `reply_to_message_id = 100`

### Requirement: Loop command and cadence
The system SHALL provide `/loop [interval] <prompt-or-slash-command> [--times N] [--until <condition>]` (alias `/proactive`) plus `/loop status`, `/loop pause`, `/loop resume` and `/loop stop`; setting a loop SHALL replace any existing loop and fire the first wakeup at the next idle boundary; a given interval (`30s`, `5m`, `2h`, `1h30m`) SHALL be fixed cadence and SHALL be refused below `loops.min_interval_secs` (default 30); an omitted interval SHALL be self-paced, starting at `loops.self_paced_floor_secs` (default 60), doubling while the reply digest (ignoring timestamps) is unchanged up to `loops.self_paced_ceiling_secs` (default 900), and snapping back to the floor when the reply changes.
**Milestone:** M2

#### Scenario: Interval below the floor
- **WHEN** the user runs `/loop 10s check the queue`
- **THEN** the system SHALL refuse with `interval must be at least 30s (loops.min_interval_secs)`

#### Scenario: Self-paced backoff
- **GIVEN** `/loop keep an eye on the migration` and three consecutive identical replies
- **WHEN** the next cadence is computed
- **THEN** the wait SHALL be 480 seconds, and SHALL return to 60 seconds after a different reply

### Requirement: Loop stop conditions
A loop SHALL end when the agent's reply ends with a line `LOOP_COMPLETE`, when `--times N` wakeups have fired, or on `/loop stop`; with `--until <condition>` the system SHALL ask `goal_judge` after each wakeup whether the condition holds (stopping on yes, pausing with the reason when the judge rules it unachievable, failing open to continue on judge error); the system SHALL pause a loop after `loops.max_ticks` wakeups (default 100, 0 = unlimited) and SHALL pause it when the user interrupts a wakeup turn.
**Milestone:** M2

#### Scenario: Run cap
- **GIVEN** `/loop 2m poll CI --times 3`
- **WHEN** the third wakeup turn ends
- **THEN** the loop SHALL be removed and `/loop status` SHALL report no loop

#### Scenario: Interrupt pauses
- **WHEN** the user presses Ctrl+C during a loop wakeup turn
- **THEN** the turn SHALL be interrupted and the loop status SHALL be `paused`

### Requirement: Session heartbeat
The system SHALL provide `/heartbeat every <interval> <prompt>` (alias `/hb`; interval minimum 60 s, units `s`, `m`, `h`, `d`), `/heartbeat status`, `/heartbeat pause`, `/heartbeat resume` and `/heartbeat clear`; a heartbeat SHALL fire only when the session is idle and the interval has elapsed, SHALL coalesce any number of missed intervals into one fire, SHALL re-anchor its timer on every fire and on resume (never firing immediately on resume), and SHALL prefix the injected prompt with an instruction to reply briefly and stop when nothing meaningful changed.
**Milestone:** M2

#### Scenario: Missed ticks coalesce
- **GIVEN** `/heartbeat every 10m check CI` and a session busy for 35 minutes
- **WHEN** the session becomes idle
- **THEN** exactly one heartbeat turn SHALL run and the next fire SHALL be 10 minutes later

#### Scenario: Minimum interval
- **WHEN** the user runs `/heartbeat every 30s ping`
- **THEN** the system SHALL refuse with `heartbeat interval must be at least 60s`

### Requirement: Heartbeat ownership, recovery and silence on the gateway
A heartbeat SHALL be owned by the surface that set it (`owner_surface`): one set from a messaging chat SHALL fire from the gateway and reply into that chat even while the session is open in the TUI or desktop; on daemon start the gateway SHALL restore active heartbeats and loops inside each owning ProfileScope using the persisted conversation and thread route, retrying on later polls after storage or adapter failures; a heartbeat turn whose final reply is exactly `NO_REPLY` or `[SILENT]` SHALL deliver nothing, and heartbeat turns SHALL post no typing indicator, tool progress or streamed draft while approval prompts and failures stay visible; a tick reserved at admission SHALL be refunded when the attempt is rejected before the turn starts.
**Milestone:** M2

#### Scenario: Silent heartbeat
- **GIVEN** a Slack heartbeat whose turn ends with the reply `NO_REPLY`
- **WHEN** the turn completes
- **THEN** no message SHALL be sent to Slack

#### Scenario: Restored after restart in its profile
- **GIVEN** an active heartbeat on profile `work` and a daemon restart
- **WHEN** the daemon starts
- **THEN** the heartbeat SHALL be rescheduled under the `work` ProfileScope without any new chat message

### Requirement: Automation status on every surface
The daemon SHALL publish an `automation.state` RPC event (declared in the workspace-architecture catalog) whenever a goal, loop or heartbeat changes or a wakeup is scheduled, carrying kind, status, counters and `next_fire_at`; the CLI and TUI SHALL render a live row (for example `loop · every 5m · 2 runs · next in 4m`) and the desktop a card with a per-second countdown; a session with an active loop or heartbeat SHALL be exempt from the disconnected-session reaper until the automation stops, pauses or exhausts its budget.
**Milestone:** M2

#### Scenario: Desktop closed, loop keeps firing
- **GIVEN** a desktop session with an active `/loop 5m` and the desktop app closed while the daemon runs
- **WHEN** 15 minutes pass
- **THEN** three wakeups SHALL have run and their replies SHALL be in the session when the app reconnects

### Requirement: Automation configuration
The system SHALL read `[goals]` (`max_turns = 20`, `gate_timeout_secs = 300`, `gate_max_retries = 3`, `max_wait_secs = 1800`), `[loops]` (`min_interval_secs = 30`, `max_ticks = 100`, `self_paced_floor_secs = 60`, `self_paced_ceiling_secs = 900`) and `[heartbeat]` (`min_interval_secs = 60`) from the profile `config.toml`, SHALL route the judge, contract drafting and `--until` checks through auxiliary task `goal_judge` (default: the main model), and SHALL fail validation with exit 78 for out-of-range values (including `self_paced_floor_secs > self_paced_ceiling_secs`) rather than clamping.
**Milestone:** M2

#### Scenario: Inverted self-paced bounds
- **GIVEN** `loops.self_paced_floor_secs = 600` and `loops.self_paced_ceiling_secs = 300`
- **WHEN** `prometheus config check` runs
- **THEN** it SHALL report the conflict naming both keys and exit 78

### Requirement: Automation is distinct from kanban and cron
Setting, pausing, resuming or clearing a goal, loop or heartbeat SHALL never create, claim or move a kanban task and SHALL never create a scheduled job; a kanban task created with goal mode SHALL reuse the goal judge loop inside its worker session only, and cron jobs SHALL remain the mechanism for unattended schedules that need their own session.
**Milestone:** M2

#### Scenario: Goal does not touch the board
- **GIVEN** a kanban board with no tasks
- **WHEN** the user sets and completes a goal
- **THEN** the board SHALL still contain no tasks

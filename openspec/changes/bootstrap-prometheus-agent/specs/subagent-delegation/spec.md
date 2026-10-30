# Subagent Delegation

## Purpose
Subagent delegation lets a session spawn isolated child agents that each run their own turn loop, session and terminal under a capability grant that can only narrow the parent's, and return a typed result instead of their full transcript. The key decision is that the host owns the whole child lifecycle (spawn, grant derivation, budgets, background completion, progress streaming, stop, result assembly) behind one contract shared by the `delegate_task` tool, slash commands, gateway RPC and the plugin lifecycle interface, and that programmatic tool calling (`execute_code`) runs under exactly the same grant, never more.

## ADDED Requirements

### Requirement: delegate_task tool contract
The system SHALL register a `delegate_task` tool in the `delegation` toolset whose input has `action` (`spawn` default, `list`, `status`, `steer`, `stop`, `wait`), `subagent_id`, `message`, `join` and `budget`, and a `tasks` array (1 to `delegation.max_tasks_per_call` items) whose items require `goal` and accept `context`, `agent`, `toolsets`, `model`, `output_schema`, `images` and `group`, and SHALL reject any other task field with a tool error naming the field.
**Milestone:** M2

#### Scenario: Unknown task field rejected
- **GIVEN** a model tool call whose task carries `acp_command`
- **WHEN** `delegate_task` validates its input
- **THEN** the system SHALL return the tool error `Task 0 has unknown field 'acp_command'.` and spawn no child

#### Scenario: Single task batch
- **GIVEN** a parent session with the `delegation` toolset
- **WHEN** the model calls `delegate_task` with `tasks = [{"goal": "Fix the failing tests in crates/store"}]`
- **THEN** the system SHALL spawn exactly one child

### Requirement: Spawn preconditions and refusals
The system SHALL refuse a spawn without building any child, returning a tool error with a stable code, when: spawning is paused for the profile (`delegation.paused`, message `Delegation spawning is paused. Resume with /delegation resume.`); the caller's depth is at least `delegation.max_spawn_depth` (`delegation.depth_limit`, message `Delegation depth limit reached (depth=N, max_spawn_depth=M).`); the session is closing (`delegation.session_closed`); or the root lineage's spawn budget or cost budget is exhausted (`delegation.budget_exhausted`); control actions SHALL bypass the pause and depth checks.
**Milestone:** M2

#### Scenario: Depth limit reached
- **GIVEN** `delegation.max_spawn_depth = 1` and a calling child at depth 1
- **WHEN** it calls `delegate_task` with a task
- **THEN** the system SHALL return a tool error with code `delegation.depth_limit` and build no child

#### Scenario: Pause blocks only new spawns
- **GIVEN** spawning paused via `/delegation pause`
- **WHEN** the model calls `delegate_task(action="list")` and then a spawn
- **THEN** the list SHALL succeed and the spawn SHALL fail with `delegation.paused`

### Requirement: Task validation
The system SHALL reject a whole call before any child is built when a task has a blank `goal`, a goal longer than 16000 characters, a `context` longer than 32000 characters, a placeholder goal (`todo`, `task N`, or an unexpanded template marker of two or more words in angle or curly brackets), more than 8 `images`, an `output_schema` that is not a valid JSON Schema (draft 2020-12), or an unknown `agent` name, naming the task index and the violated rule in the tool error; brackets around code-like content such as `Vec<T>` or `{"key": 1}` SHALL NOT count as template markers.
**Milestone:** M2

#### Scenario: Ninth image rejected
- **GIVEN** a task with 9 image references
- **WHEN** `delegate_task` validates the call
- **THEN** the system SHALL return `Task 0 has 9 images; the per-task limit is 8.` and spawn no child

#### Scenario: Code-shaped brackets accepted
- **GIVEN** a task goal `Implement Display for Vec<T> wrappers`
- **WHEN** the call is validated
- **THEN** the system SHALL accept the goal

### Requirement: Delegation configuration defaults
The system SHALL read the `[delegation]` table of the profile's `config.toml` with defaults `max_concurrent_children = 8` (range 1-64, one limit shared by synchronous and background children across the profile), `max_tasks_per_call = 8` (range 1-64), `max_spawn_depth = 1` (range 1-4), `max_iterations = 150` (range 1-1000), `inactivity_timeout_secs = 0` (0 disables, otherwise minimum 30), `max_summary_chars = 24000` (0 disables), `child_approval = "deny"`, `background = "auto"`, `oneshot_max_children = 2` (0 unlimited) and `progress_events = true`, and SHALL fail config validation for an out-of-range or wrongly typed value instead of substituting a default.
**Milestone:** M2

#### Scenario: Invalid value fails validation
- **GIVEN** `[delegation] max_concurrent_children = "lots"`
- **WHEN** the profile config is loaded
- **THEN** the system SHALL fail validation naming `delegation.max_concurrent_children` and its accepted range

#### Scenario: Defaults documented from one source
- **GIVEN** the generated configuration reference
- **WHEN** it is compared with `prometheus config schema --json`
- **THEN** every `delegation.*` default SHALL be identical in both

### Requirement: Role derived from depth only
The system SHALL derive a child's role from depth alone: a child at depth `d` SHALL be an `orchestrator` (keeps the `delegation` toolset) when `d < delegation.max_spawn_depth`, otherwise a `leaf` (has no `delegation` toolset), and no tool, slash command, RPC or plugin interface SHALL accept a caller-supplied role.
**Milestone:** M2

#### Scenario: Flat default yields leaf children
- **GIVEN** default config (`max_spawn_depth = 1`)
- **WHEN** a depth-0 session spawns a child
- **THEN** the child SHALL be a leaf whose tool list has no `delegate_task`

### Requirement: Child capability grant can only narrow (upgrade)
The system SHALL compute a child's capability grant as the intersection of the parent's effective grant with the narrowing requested by the task, the named agent definition and the delegation config, where toolsets on every side are first expanded to concrete tool names (composite toolsets and MCP server toolsets included), and the grant covers tools, filesystem paths, network hosts, secrets by name, approval policy and budgets; a request that names any tool, path, host or secret absent from the parent's grant SHALL be refused with code `delegation.grant_broadening` listing every offending item, and the tools `clarify`, `memory`, `send_message`, `cronjob` and `kanban` SHALL always be removed from a child's grant.
**Milestone:** M2

#### Scenario: Composite narrowing is not falsely rejected
- **GIVEN** a parent whose toolsets are `["coding"]`, a composite expanding to `file`, `terminal` and `web`
- **WHEN** a task requests `toolsets = ["file", "terminal"]`
- **THEN** the system SHALL accept it and give the child exactly the tools of `file` and `terminal`

#### Scenario: Broadening refused
- **GIVEN** a parent without the `browser` toolset
- **WHEN** a task requests `toolsets = ["browser"]`
- **THEN** the system SHALL refuse that task with `delegation.grant_broadening` listing every browser tool name

#### Scenario: Child inherits the parent's filesystem limits
- **GIVEN** a parent whose grant allows writes only under `/work/repo`
- **WHEN** its child runs `write_file` on `/etc/hosts`
- **THEN** the system SHALL deny the write as outside the grant

### Requirement: Child session isolation
The system SHALL run every child as a new session in the parent's `ProfileScope` (unless a profile route applies) with `parent_session_id` and `root_session_id` set, a `subagent_id` of the form `sa-<26-char ULID>`, its own iteration budget and terminal session, no context files, no clarify channel, a read-only view of the parent's memory snapshot taken at the parent's session start, and an ephemeral system prompt built from the goal, context and agent definition, and SHALL never share the parent's mutable conversation state.
**Milestone:** M2

#### Scenario: Child cannot write memory
- **GIVEN** a child spawned from a session with memory enabled
- **WHEN** the child's tool list is built
- **THEN** it SHALL contain no memory-writing tool and its prompt SHALL include the parent's frozen memory snapshot

### Requirement: Named subagent definitions (upgrade)
The system SHALL load named subagent definitions from `<profile home>/agents/<name>.md` and from the active harness, each a Markdown file with front matter `name` (equal to the file stem), `description` (required, at most 600 characters), and optional `toolsets`, `model`, `max_iterations`, `output_schema` and `background`, the body becoming the child's system prompt; it SHALL list each definition's name and description in the `delegate_task` tool description, SHALL freeze the set of definitions at session start, and SHALL fail with `delegation.unknown_agent` for a task naming an undefined agent.
**Milestone:** M2

#### Scenario: Definition added mid-session
- **GIVEN** a running session and a new file `agents/reviewer.md` created during it
- **WHEN** the model calls `delegate_task` with `agent = "reviewer"`
- **THEN** the system SHALL refuse it with `delegation.unknown_agent`, and a new session SHALL accept it

#### Scenario: Mismatched name rejected at load
- **GIVEN** `agents/reviewer.md` whose front matter says `name: critic`
- **WHEN** definitions are loaded
- **THEN** the system SHALL report the file as invalid naming both values and SHALL NOT register either name

### Requirement: Child model routing
The system SHALL run a child on the first of: the task's `model`; the agent definition's `model`; `delegation.model`; the parent's current route, and SHALL give a child that pins its own model no inherited fallback chain while an unpinned child inherits the parent's chain, as defined by model-providers; a pinned model that fails route validation SHALL refuse only that task.
**Milestone:** M2

#### Scenario: Pinned child does not inherit fallback
- **GIVEN** a parent with a fallback chain and a task with `model = "openrouter/cheap-model"`
- **WHEN** the child's route fails with `overloaded`
- **THEN** the system SHALL NOT activate the parent's fallback chain for that child

### Requirement: Child approvals fail closed (upgrade)
The system SHALL resolve an approval needed inside a child per `delegation.child_approval`: `deny` (default) denies the action and returns the denial to the child; `escalate` forwards the approval request to the root session's surface tagged with the `subagent_id` and denies it if nobody answers within `delegation.escalation_timeout_secs` (default 300); there SHALL be no setting that auto-approves dangerous actions in children.
**Milestone:** M2

#### Scenario: Default deny
- **GIVEN** default config
- **WHEN** a child's terminal command requires approval
- **THEN** the system SHALL deny it, return a denial tool result to the child, and log the denial with the `subagent_id`

#### Scenario: Escalation times out to deny
- **GIVEN** `child_approval = "escalate"` and a user who does not respond
- **WHEN** 300 seconds elapse after the request is shown
- **THEN** the system SHALL deny the action

### Requirement: Background and synchronous dispatch
The system SHALL dispatch a depth-0 spawn in the background when `delegation.background = "auto"` and the session can receive later completions (interactive CLI, TUI, desktop, gateway chats), returning immediately `{"status": "dispatched", "delegation_id": "dg-<ULID>", "subagent_ids": [...]}` and delivering the result later as a new turn input of type `delegation_completed`; it SHALL run the call synchronously, adding a `note` saying why, for nested spawns, one-shot runs, cron jobs, kanban workers and stateless API calls, and when `background = "never"`.
**Milestone:** M2

#### Scenario: Background dispatch returns a handle
- **GIVEN** an interactive depth-0 session
- **WHEN** the model spawns two tasks
- **THEN** the tool result SHALL have `status = "dispatched"` and a `delegation_id` prefixed `dg-` without waiting for the children

#### Scenario: One-shot run is synchronous
- **GIVEN** `prometheus chat -z "summarise the repo"`
- **WHEN** the model spawns a task
- **THEN** the call SHALL block until the child finishes and include the result inline

### Requirement: Concurrency limit and fair queueing
The system SHALL never run more than `delegation.max_concurrent_children` children at once per profile, admitting children in FIFO order per root session with round-robin across root sessions, SHALL queue a spawn that exceeds the limit up to `delegation.max_queued` (default 32) with status `queued`, and SHALL refuse further spawns with code `delegation.at_capacity` once the queue is full.
**Milestone:** M2

#### Scenario: Excess children queue rather than oversubscribe
- **GIVEN** `max_concurrent_children = 2` and a call with 3 tasks
- **WHEN** the call is dispatched
- **THEN** two children SHALL run, the third SHALL report `queued`, and it SHALL start when one finishes

### Requirement: Durable background completions
The system SHALL record each background delegation and its children in the profile's `state.db` (`delegations` and `subagents` tables), SHALL deliver each completion exactly once to its owning session, SHALL on restart mark children that were running as `interrupted` and deliver their completions, SHALL drop undelivered completions older than 48 hours with a WARN, and SHALL stop retrying delivery after 8 attempts, marking the row `dropped`.
**Milestone:** M2

#### Scenario: Restart delivers an owned completion once
- **GIVEN** a background child that completed while its session's surface was disconnected, followed by a gateway restart
- **WHEN** the session resumes
- **THEN** the completion SHALL be delivered once and its row marked `delivered`

### Requirement: Stall detection instead of wall-clock timeouts
The system SHALL impose no default wall-clock limit on a child and SHALL instead treat it as stalled after 450 seconds without progress between iterations or 1200 seconds inside one tool call, or after `delegation.inactivity_timeout_secs` when set; a stalled child SHALL be interrupted, given a 120-second grace period, then finalised with `status = "stalled"` and the observed quiet time.
**Milestone:** M2

#### Scenario: Progressing child never stalls
- **GIVEN** a child that completes an iteration every 60 seconds for 2 hours
- **WHEN** the stall monitor samples it
- **THEN** the system SHALL NOT interrupt it

### Requirement: Control actions scoped to the caller's spawn tree
The system SHALL let a session `list`, `status`, `steer`, `stop` and `wait` only on children in its own spawn tree (descendants by `parent_session_id` across compression lineage), returning `No live subagent '<id>' in this conversation's spawn tree.` for any other id; `steer` SHALL require a non-empty `message`, queue it for the child's next iteration and return `queued`, a steer that never lands SHALL appear as `missed_steer` on the result, and `stop` SHALL request a cooperative interrupt and return `interrupt_requested`.
**Milestone:** M2

#### Scenario: Foreign child cannot be steered
- **GIVEN** a running child spawned by another session
- **WHEN** this session calls `delegate_task(action="steer", subagent_id=<that id>, message="focus on tests")`
- **THEN** the system SHALL return the not-in-spawn-tree error

### Requirement: Stop semantics
The system SHALL propagate a turn interrupt (Ctrl+C, Esc, stop button, interrupting message, ACP cancel) to synchronous children of that turn only, SHALL leave background children running across turn interrupts, and SHALL stop all of a session's children on `/stop all`, session close or `/new`, after which spawns for the closed session fail with `delegation.session_closed`.
**Milestone:** M2

#### Scenario: Turn interrupt leaves background work running
- **GIVEN** a background child running for a session
- **WHEN** the user presses Esc during the next turn
- **THEN** the background child SHALL continue and later deliver its completion

### Requirement: Live child progress streaming (upgrade)
The system SHALL publish `subagent.spawned`, `subagent.progress` and `subagent.finished` events on the root session's event stream and trajectory when `delegation.progress_events = true`, where `subagent.progress` carries `subagent_id`, `parent_id`, `depth`, `iteration`, the current tool name, a one-line activity summary of at most 120 characters, cumulative tokens and cost, and is rate-limited to 4 events per second per child with the latest state coalesced; every surface SHALL render the live spawn tree from these events, and the events SHALL NOT be inserted into the parent model's context.
**Milestone:** M2

#### Scenario: Progress coalesced under load
- **GIVEN** a child that starts 40 tool calls within one second
- **WHEN** progress events are published
- **THEN** at most 4 `subagent.progress` events SHALL be emitted for that second and the last SHALL reflect the 40th call

#### Scenario: Parent context untouched
- **GIVEN** a background child emitting progress events
- **WHEN** the parent takes its next turn
- **THEN** the request sent to the model SHALL contain no progress event content

### Requirement: Structured result contract (upgrade)
The system SHALL return one `SubagentResult` (contract version 1) per task with `subagent_id`, `task_index`, `status` (`completed`, `failed`, `interrupted`, `stalled`, `refused`), `exit_reason` (`final_answer`, `max_iterations`, `budget_exhausted`, `interrupted`, `error`), `truncated`, `summary`, `output` (the parsed JSON when an `output_schema` was given, else null), `schema_valid`, `schema_errors`, `artifacts` (paths written, with SHA-256 and a `blobs/<sha256>` reference for each file at most 1 MiB), `usage` (tokens by kind, iterations, tool calls), `cost_usd`, `cost_status`, `duration_ms`, `model`, `error` (code and message when failed) and `trajectory_ref`, and SHALL publish its JSON Schema through `prometheus schema subagent-result`.
**Milestone:** M2

#### Scenario: Budget exhaustion is truncation, not failure
- **GIVEN** a child that reached its iteration budget with a non-empty final answer
- **WHEN** its result is built
- **THEN** it SHALL have `status = "completed"`, `exit_reason = "max_iterations"` and `truncated = true`

#### Scenario: Artifacts reported
- **GIVEN** a child that wrote `src/lib.rs` and `README.md`
- **WHEN** its result is built
- **THEN** `artifacts` SHALL list both paths with their SHA-256 digests

### Requirement: output_schema validation never discards results
The system SHALL give a child its `output_schema` in its prompt, ask it to end with a JSON answer, validate that answer, allow exactly one correction attempt on failure, and on a second failure SHALL keep `status = "completed"` with the raw text in `summary`, `output = null`, `schema_valid = false` and `schema_errors` populated; failed children SHALL skip validation.
**Milestone:** M2

#### Scenario: Invalid twice
- **GIVEN** a child whose answer violates its schema on the first attempt and on the correction
- **WHEN** its result is built
- **THEN** it SHALL carry `schema_valid = false`, the errors, and the raw answer as `summary`

### Requirement: Summary budget protects the parent context
The system SHALL cap each result's `summary` inserted into the parent's context at the smaller of `delegation.max_summary_chars` and 50% of the parent's remaining context window divided across the call's tasks (floor 2000 characters), keeping the first 75% and last 25% of the allowed length with a `[SUMMARY TRUNCATED]` marker, and SHALL store the full summary as a blob referenced by `summary_ref` that the parent can read with `read_file`.
**Milestone:** M2

#### Scenario: Oversized summary is kept losslessly
- **GIVEN** a child summary of 100000 characters and default config
- **WHEN** the result is inserted into the parent's context
- **THEN** the inserted summary SHALL be at most 24000 characters and `summary_ref` SHALL resolve to the full 100000 characters

### Requirement: Fan-out budget split and roll-up (upgrade)
The system SHALL accept an optional call-level `budget` of `max_cost_usd`, `max_tokens` and `max_iterations`, divide it across the call's tasks per `budget.split` (`even` default, or `weighted` using per-task `weight`), enforce each child's share as a hard limit ending the child with `exit_reason = "budget_exhausted"`, cap any child budget so the sum never exceeds the root lineage's remaining session budget, and roll every child's tokens and cost into the parent's usage ledger under the root lineage.
**Milestone:** M2

#### Scenario: Even split enforced
- **GIVEN** a call with `budget.max_cost_usd = 1.00` and 4 tasks
- **WHEN** one child reaches $0.25 of spend
- **THEN** the system SHALL stop that child with `exit_reason = "budget_exhausted"` while the others continue

#### Scenario: Child cost appears in the root session total
- **GIVEN** a parent that spent $0.40 and two children that spent $0.10 each
- **WHEN** the user runs `prometheus usage --session <root id>`
- **THEN** the total SHALL be $0.60 with the children itemised

### Requirement: Fan-in join modes (upgrade)
The system SHALL support a call-level `join` of `all` (default: wait for every child), `first_success` (return when the first child completes with `status = "completed"` and, when an `output_schema` is set, `schema_valid = true`, then stop the rest) and `quorum:N` (return when N children have completed successfully, then stop the rest), reporting stopped children with `status = "interrupted"` and `exit_reason = "interrupted"`, and SHALL return `join_satisfied = false` when the mode cannot be met.
**Milestone:** M3

#### Scenario: First success cancels the rest
- **GIVEN** a call with three tasks and `join = "first_success"`
- **WHEN** the second task completes successfully first
- **THEN** the system SHALL return its result, stop the other two, and report them as interrupted

#### Scenario: Quorum not reachable
- **GIVEN** `join = "quorum:2"` and three children of which two fail
- **WHEN** all children have finished
- **THEN** the system SHALL return all three results with `join_satisfied = false`

### Requirement: One-shot spawn budget
The system SHALL charge spawns against `delegation.oneshot_max_children` only in finite one-shot runs (`prometheus chat -z`, cron jobs, eval runs), refusing further spawns with `delegation.budget_exhausted` and the message `Delegation budget for this run is exhausted (N/M subagents used; delegation.oneshot_max_children).`, and SHALL never charge interactive or gateway sessions.
**Milestone:** M2

#### Scenario: Third child in a one-shot run
- **GIVEN** a one-shot run that already spawned 2 children under default config
- **WHEN** it spawns again
- **THEN** the system SHALL refuse with `delegation.budget_exhausted`

### Requirement: Delegation routers fail closed
The system SHALL select a delegation router from `delegation.router` (default `default`, built in), where a plugin router may add typed task parameters (names matching `^[a-z][a-z0-9_]{0,63}$`, not colliding with built-in task fields) and choose per task a model, profile or remote backend; an unknown or unavailable router SHALL make every spawn fail with code `delegation.router_unavailable` naming the router and SHALL never fall back to `default`, and a router refusing one task SHALL refuse only that task with `status = "refused"` while its siblings run.
**Milestone:** M2

#### Scenario: Missing router
- **GIVEN** `delegation.router = "tiers"` and no enabled plugin providing `tiers`
- **WHEN** the model spawns a task
- **THEN** the system SHALL fail with `delegation.router_unavailable` and spawn no child

#### Scenario: One unroutable task
- **GIVEN** a router that refuses task 1 of 3
- **WHEN** the call runs
- **THEN** tasks 0 and 2 SHALL run and task 1 SHALL have `status = "refused"` with the router's message

### Requirement: Delegation settings changes respect prompt caching
The system SHALL freeze the `delegate_task` schema, router, agent definitions and delegation config for the life of a session, so `/delegation router <name>` and edits to `[delegation]` apply to new sessions only unless `--now` is given, in which case the session's tool schemas are rebuilt before its next turn and a `session.tools_rebuilt` event is emitted.
**Milestone:** M2

#### Scenario: Plain switch leaves the running session unchanged
- **GIVEN** a running session built with router `default`
- **WHEN** the user runs `/delegation router tiers` without `--now`
- **THEN** this session SHALL keep routing with `default` and the next new session SHALL use `tiers`

### Requirement: Profile routes run the child inside the target ProfileScope
The system SHALL, when a router or task selects a `profile`, refuse a non-existent profile with `Profile '<name>' does not exist (see 'prometheus profile list').`, build and run the child entirely within the target profile's `ProfileScope` (config, secrets store, sandbox policy), never resolve a secret from the launching profile, keep the child's session record in the launching profile's `state.db`, and intersect the child's grant with both profiles' grants.
**Milestone:** M2

#### Scenario: No launching-profile secret leak
- **GIVEN** a target profile without an `OPENROUTER_API_KEY` secret while the launching profile has one
- **WHEN** a child is routed to the target profile on `openrouter`
- **THEN** the child SHALL fail with `provider.missing_credential`

### Requirement: Subagent lifecycle interface for plugins
The system SHALL expose a host `subagents` interface to plugins that hold the `subagents` capability, offering `launch`, `status`, `wait`, `steer`, `cancel` and `result` over the same host-owned path, validation and grant narrowing as `delegate_task`, issuing handles that carry an HMAC over `subagent_id`, `root_session_id` and creation time so forged or foreign handles yield `unknown_handle`, using the states `queued`, `running`, `completed`, `failed`, `interrupted`, `stalled`, `cancel_requested` and `cancelled` (each reachable), and returning `SubagentResult` values that are immutable once terminal and retained for 3600 seconds after completion.
**Milestone:** M2

#### Scenario: Plugin without the capability
- **GIVEN** a plugin whose `plugin.toml` does not declare `subagents`
- **WHEN** it calls `launch`
- **THEN** the system SHALL return `capability_denied` and spawn no child

#### Scenario: Forged handle
- **GIVEN** a handle whose HMAC was altered
- **WHEN** `status(handle)` is called
- **THEN** the system SHALL return `unknown_handle`

### Requirement: Child processes are reaped or handed off
The system SHALL terminate a child's background processes and code-execution kernels when the child finishes, unless the child hands a process off with `process(action="handoff")`, which transfers ownership and its completion notices to the parent; processes that could not be terminated within 10 seconds SHALL be listed as `orphaned_processes` on the result.
**Milestone:** M2

#### Scenario: Handed-off process survives the child
- **GIVEN** a child that starts a dev server and hands it off
- **WHEN** the child finishes
- **THEN** the server SHALL keep running owned by the parent and its exit notice SHALL go to the parent

### Requirement: Programmatic tool calling under the caller's grant (upgrade)
The system SHALL provide an `execute_code` tool that runs a Python script (via the managed Python runtime) in the OS sandbox derived from the calling session's capability grant, exposing tool stubs for exactly the intersection of `code_execution.allowed_tools` (default `read_file`, `write_file`, `search_files`, `patch`, `terminal`, `web_search`, `web_extract`) with the caller's granted tools; when that intersection is empty the system SHALL refuse with code `execute_code.no_tools` and SHALL NOT expose any other tool, and every stub call SHALL be re-checked against the grant by the host.
**Milestone:** M2

#### Scenario: Empty intersection never widens
- **GIVEN** a child whose grant contains only `read_file`, and `code_execution.allowed_tools = ["terminal"]`
- **WHEN** the child calls `execute_code`
- **THEN** the system SHALL refuse with `execute_code.no_tools` and run no code

#### Scenario: Stub limited to the grant
- **GIVEN** a session granted `read_file` and `search_files` only
- **WHEN** a script calls the `terminal` stub
- **THEN** the system SHALL return `Tool 'terminal' is not available in execute_code. Available: read_file, search_files` without consuming the call budget

#### Scenario: Script sandboxed like the session
- **GIVEN** a session whose grant forbids network access
- **WHEN** an `execute_code` script opens a socket to an external host
- **THEN** the sandbox SHALL block the connection

### Requirement: execute_code RPC channel and limits
The system SHALL connect the script to the host over a per-execution channel authenticated by a 256-bit random token compared in constant time (an empty or wrong token SHALL be rejected with `Unauthorized RPC request`), SHALL limit each execution to `code_execution.max_tool_calls` stub calls (default 50) and `code_execution.timeout_secs` (default 300), SHALL cap captured stdout at 50000 bytes (40% head, 60% tail, full output stored as a blob) and stderr at 10000 bytes, SHALL start the script with an empty environment plus only `PATH`, `HOME` (a scratch directory), `LANG` and the RPC endpoint variables, and SHALL strip background, PTY and notification options from `terminal` stub calls.
**Milestone:** M2

#### Scenario: Secrets absent from the script environment
- **GIVEN** a profile with an `OPENAI_API_KEY` secret
- **WHEN** a script prints its full environment
- **THEN** the output SHALL contain no secret name or value

#### Scenario: Call budget enforced
- **GIVEN** default config
- **WHEN** a script makes its 51st stub call
- **THEN** the system SHALL return `Tool call limit reached (50).` for that call and later calls

### Requirement: execute_code pre-execution guards
The system SHALL refuse `execute_code` without running anything when `code` is empty or whitespace (`No code provided.`), when the session's sandbox policy cannot be resolved (`execute_code refused: sandbox policy unavailable`), when the script contains a command that stops or restarts the gateway serving the session, or when the approval policy for the session does not approve the script.
**Milestone:** M2

#### Scenario: Empty code
- **GIVEN** an `execute_code` call with whitespace-only `code`
- **WHEN** it is handled
- **THEN** the system SHALL return `No code provided.` and run nothing

### Requirement: User-launched background sessions
The system SHALL provide `/bg <prompt>` on every surface that starts a new, independent session (own `ses_` id, no history from the current session, `source = background`, `parent_session_id` set to the launching session for lineage only) inheriting the launching session's profile, model route, toolsets, reasoning settings and fallback chain, SHALL return immediately with `Background task #<n> started` and a task id `bg-<ULID>`, SHALL deliver the final answer (or failure) to the launching surface as a labelled panel (CLI/TUI/desktop) or chat message (gateway) without inserting it into the launching session's history, SHALL count background sessions against `delegation.max_concurrent_children`, and SHALL let `/agents` list and stop them.
**Milestone:** M2

#### Scenario: Result delivered to the same chat
- **GIVEN** a Telegram chat
- **WHEN** the user sends `/bg summarise today's error logs` and keeps chatting
- **THEN** the foreground replies SHALL continue and, when the background session ends, its answer SHALL be posted to the chat labelled with its task number, and the foreground session's transcript SHALL not contain it

### Requirement: Agents monitor and live transcripts
The system SHALL provide `/agents` (alias `/tasks`) listing for the current session its running and recently finished subagents as a tree grouped by parent, its `/bg` background sessions, its active goal, loop and heartbeat, and its background processes, each with status, elapsed time, model, tokens and cost; the TUI and desktop SHALL offer inspect (turn-by-turn history, available after the child returns), steer and stop per item, stopping one child SHALL not affect its siblings; every dispatched child SHALL also write an append-only human-readable transcript at `<profile>/cache/delegation/live/<delegation_id>/task-<n>.log` plus a `manifest.json`, whose paths SHALL be returned in the dispatch result as `live_transcripts`, and directories older than 7 days SHALL be pruned on later dispatches.
**Milestone:** M2

#### Scenario: Stop one child
- **GIVEN** a background call with three running children
- **WHEN** the user stops the second from `/agents`
- **THEN** that child SHALL finish with `status = "interrupted"` and the other two SHALL keep running

#### Scenario: Tail a live transcript
- **WHEN** a child is dispatched
- **THEN** its `task-<n>.log` SHALL exist before the child's first model call and SHALL gain one timestamped line per tool call

### Requirement: Review command
The system SHALL provide `/review [instructions]` on every surface that dispatches a background child on the delegation rail whose evidence is a snapshot of the last 10 user and assistant messages of the session (tool output and system messages excluded, read from storage when no agent is resident), with the child's normal toolset under the session's grant, a briefing naming the skills the session had loaded, and the instructions as focus; the reviewer's model SHALL resolve from `[delegation.review]` `provider` and `model` (default: the main route); the dispatch SHALL print `Review started. Results will return here.` and the completion SHALL re-enter the session as a normal `delegation_completed` input labelled `Review: <focus>` or `Review recent work`.
**Milestone:** M2

#### Scenario: Review returns to the session
- **GIVEN** a session whose agent just opened a pull request
- **WHEN** the user runs `/review focus on security`
- **THEN** a reviewer child SHALL run in the background and its findings SHALL arrive as a `delegation_completed` input to the same session

### Requirement: Independent completions
The system SHALL, when `delegation.independent_completions = true` (default false), advertise an optional per-task `group` field in the `delegate_task` schema and deliver each ungrouped task's result as its own completion as soon as it finishes and each group's results together once all its tasks finish, with unit ids `<delegation_id>-1`, `-2`, … listed in the dispatch handle as `units[]` (`delegation_id`, `group`, `task_indexes`); grouping SHALL change delivery only, never execution order or concurrency accounting; with the setting off a `group` field SHALL be accepted and ignored, and changing the setting SHALL apply from the next session.
**Milestone:** M2

#### Scenario: Ungrouped results arrive separately
- **GIVEN** `delegation.independent_completions = true` and a call with two ungrouped tasks and two tasks in group `bench`
- **WHEN** all four finish at different times
- **THEN** the session SHALL receive three completions: one per ungrouped task and one for `bench`

### Requirement: Child request overrides and direct endpoints
The system SHALL accept `delegation.base_url` with `delegation.api_key_secret` (the name of a secret in the secrets store) and `delegation.wire_format` (`chat_completions` | `responses` | `anthropic_messages`, default inferred from the URL) as a direct child endpoint alternative to `delegation.provider`, and `delegation.request_overrides` (a table whose top-level keys are request parameters and whose `extra_body` sub-table is deep-merged one level into the request body) applied to every child model call on every resolution branch, with explicit keys winning over route-derived values; an override naming a parameter the wire format does not support SHALL fail config validation with exit 78.
**Milestone:** M2

#### Scenario: Routing hint on every child call
- **GIVEN** `[delegation.request_overrides.extra_body.provider] sort = "throughput"` and an inherited OpenRouter route
- **WHEN** a child makes a model call
- **THEN** the request body SHALL contain `provider.sort = "throughput"` while keeping the route's other body fields

### Requirement: Gateway delegation mode and one-shot joining
The system SHALL read `delegation.gateway_mode` (`auto` default | `inline` | `background`): `auto` makes a messaging session join its children inside the `delegate_task` call when interim assistant messages are disabled for that platform and keeps background dispatch otherwise; `inline` joins on every gateway session; `background` never joins; a joining session's `delegate_task` schema SHALL omit the live-control actions, a `/stop` or interrupting message SHALL stop its children with partial results, and the mode SHALL be fixed per session at creation (changes apply to new sessions); finite runs (`prometheus chat -z`, non-TTY stdin) SHALL always join their children before exiting, subject to the normal limits and approval policy.
**Milestone:** M2

#### Scenario: One message per turn on Telegram
- **GIVEN** `delegation.gateway_mode = "auto"` and a Telegram platform with interim messages off
- **WHEN** the model delegates two tasks
- **THEN** the turn SHALL wait for both results and send one final reply

### Requirement: Worktree isolation for children
The system SHALL, when `delegation.worktree_isolation = true` (default false) and the session uses the local terminal backend inside a git repository, start each child in its own git worktree and branch and dispose of it on finish exactly as `git-worktrees` › Subagent worktree isolation specifies (that requirement owns path, branch prefix and finish disposition: a clean tree without commits removed after a successful inspection probe; a tree with commits or edits archived to a ref and removed with its branch kept for the parent to merge), instruct the child to work and commit there, and add to the child's result `worktree: {path, branch, commits, dirty, pruned, archive_ref, inspection_failed}`; outside git or on non-local backends the setting SHALL fall back to the shared workspace with a `note` in the result.
**Milestone:** M2

#### Scenario: Clean child pruned
- **GIVEN** worktree isolation on and a research child that made no edits
- **WHEN** the child finishes and the inspection probe succeeds
- **THEN** its worktree and branch SHALL be removed and the result SHALL report `pruned: true`

#### Scenario: Failed probe keeps work
- **GIVEN** a child that made no edits and whose worktree inspection probe fails
- **WHEN** the child finishes
- **THEN** the worktree SHALL be kept in place and the result SHALL report `inspection_failed: true`

### Requirement: Persistent execute_code session kernel
The system SHALL reuse one Python kernel process per (session, execution mode, interpreter, working directory, granted tool set) across `execute_code` calls so imports and variables persist, SHALL discard it when a call passes `reset: true`, SHALL report in the result when a timeout or interrupt killed the kernel and its state was lost, SHALL fix the kernel's environment at start, SHALL re-check tool authority on every cell, SHALL reap a kernel idle for `code_execution.kernel_idle_timeout_secs` (default 1800) and evict the least recently used top-level session's kernel when more than `code_execution.max_session_kernels` (default 4) are alive (children's kernels not counted), and on remote backends SHALL run a remote kernel or fall back to one-shot scripts, saying so in the result.
**Milestone:** M2

#### Scenario: State persists between calls
- **GIVEN** a call that runs `x = 41`
- **WHEN** the next call runs `print(x + 1)`
- **THEN** the output SHALL be `42`

#### Scenario: Reset discards state
- **WHEN** a call with `reset: true` runs `print(x)`
- **THEN** the result SHALL contain a `NameError`

### Requirement: execute_code execution modes
The system SHALL read `code_execution.mode`: `project` (default) runs scripts in the session's working directory with the Python of the virtual environment active in the session's terminal environment (or a `.venv` in the working directory) when it exists and is version 3.8 or newer, falling back to the managed Python runtime; `strict` runs scripts in a fresh temporary staging directory with the managed Python runtime; both modes SHALL apply the identical sandbox, environment scrubbing, tool allow-list and limits, and a fallback SHALL be noted in the result.
**Milestone:** M2

#### Scenario: Broken venv falls back
- **GIVEN** `code_execution.mode = "project"` and a project venv whose interpreter is missing
- **WHEN** `execute_code` runs
- **THEN** the script SHALL run on the managed Python runtime and the result SHALL note the fallback

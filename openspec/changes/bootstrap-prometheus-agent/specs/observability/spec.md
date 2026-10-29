# Observability

## Purpose
Defines how Prometheus makes its behaviour visible and accountable: one trajectory grammar that every surface writes, structured logs and `prometheus logs`, opt-in OpenTelemetry traces and metrics, health endpoints, and a cost and token ledger with hard budgets. The key decision is that usage is an append-only, per-model-call ledger keyed by root lineage, so totals include auxiliary calls, compression continuations and subagents by construction (fixing Hermes' root-only undercount), and the same ledger enforces budgets before a call is made.

## ADDED Requirements

### Requirement: One trajectory grammar for every surface
The system SHALL record every session, whatever its surface (CLI, TUI, gateway adapter, cron, ACP, MCP server, subagent, eval, harness evolution), as a trajectory of JSON-lines events at `<profile home>/trajectories/<session_id>.jsonl`, where each line has exactly the top-level keys `v`, `ts` (RFC 3339 UTC with microseconds), `seq` (per-session monotonic from 1), `session_id`, `root_session_id`, `turn_id` (null outside a turn), `span_id`, `parent_span_id`, `type` and `payload`. The surface SHALL appear only as `payload.source` of the `session.start` event.
**Milestone:** M1

#### Scenario: Same script, different surface
- **GIVEN** the same scripted mock-provider conversation run once through `prometheus chat` and once through a gateway webhook adapter
- **WHEN** both trajectories are normalised by removing ids, timestamps and `payload.source`
- **THEN** the two event sequences SHALL be identical

### Requirement: Trajectory event types
The trajectory grammar SHALL define exactly these event types, each with a typed payload in the checked-in schema `schema/trajectory/v1.json`, and every other capability SHALL use these names verbatim: `session.start`, `session.end`, `session.tools_rebuilt`, `turn.start`, `turn.step`, `turn.end`, `user.message`, `assistant.message`, `assistant.reasoning`, `model.call`, `aux_call.start`, `aux_call.end`, `tool.call`, `tool.result`, `tool.batch_planned`, `approval.request`, `approval.decision`, `clarify.request`, `clarify.answer`, `subagent.spawned`, `subagent.progress`, `subagent.finished`, `compression.committed`, `compression.failed`, `prompt.rebuilt` (with a `reason` field such as `new_session`, `compaction`, `model_changed`, `skills --now`, `plugins --now`, `memory --now`, `harness --now`), `cache.unexpected_break`, `provider.fallback`, `provider.route_selected`, `provider.registry_reloaded`, `memory.recalled`, `budget.warning`, `budget.exceeded`, `egress.denied`, `security.downgrade`, `harness.note`, `harness.activated`, `evolve.job.started`, `evolve.job.finished`, `evolve.stage.finished`, `evolve.rollout.finished`, `evolve.selection.finished`, `evolve.proposal.ready`, `evolve.proposal.adopted`, `experience.entry.added`, `experience.entry.evicted`, `experience.entry.status_changed`, `experience.lesson.distilled`, `experience.session_lessons.injected`, `error`. The daemon-level events `usage.updated`, `health.changed`, `message.delivery`, `adapter.state`, `request.cancel`, `request.resolved` and `session.presence` SHALL be RPC events declared in the `workspace-architecture` catalog and not trajectory event types; `approval.request` SHALL be both a trajectory event and the RPC server request of the same name. `model.call` SHALL carry `call_id`, `provider`, `model`, `purpose`, `request_hash`, `cache_hit`, `latency_ms` and the token and cost fields of its ledger row; `tool.result` SHALL carry `ok`, `duration_ms` and either `output` or `error`.
**Milestone:** M1

#### Scenario: Tool failure recorded
- **WHEN** a `read_file` call fails because the file does not exist
- **THEN** the trajectory SHALL contain a `tool.result` with `ok: false` and `error.kind` equal to `not_found`

### Requirement: Grammar versioning and forward compatibility
The `v` field SHALL hold the grammar version `<major>.<minor>`; minor versions SHALL only add event types or optional payload fields, and readers SHALL preserve unknown event types and render them as `[EVENT <type>]` instead of failing. A reader SHALL refuse a newer major with `trajectory grammar <x> is newer than supported <y>; run 'prometheus update'` and exit 76.
**Milestone:** M1

#### Scenario: Older binary reads newer minor
- **GIVEN** a trajectory written with grammar 1.3 containing an event type unknown to a 1.1 reader
- **WHEN** the 1.1 reader renders it
- **THEN** the unknown event SHALL be rendered as `[EVENT <type>]` and the rest SHALL render normally

### Requirement: Canonical text rendering
`prometheus trajectory show <session_id> [--format text|jsonl] [--args-trunc N] [--obs-trunc N] [--max-chars N]` SHALL render the text format with the markers `[USER]`, `[ASSISTANT]`, `[THINKING]`, `[ACTION] <tool>(<args>)`, `[OBSERVATION]`, `[OBSERVATION ERROR]`, `[APPROVAL]`, `[SUBAGENT <id>]`, `[COMPRESSION]`, `[HARNESS]` and `[ERROR]`, clipping tool arguments at 2,000 characters, each observation at 50,000 characters (minimum 10,000) and the whole output at `--max-chars` (default unlimited) with the marker `…(+N chars — render truncation)`. The rendering SHALL be deterministic: the same JSONL SHALL produce byte-identical text, and this renderer SHALL be the one used by harness evolution, selection and eval.
**Milestone:** M1

#### Scenario: Oversized observation
- **GIVEN** a tool result of 80,000 characters
- **WHEN** the trajectory is rendered with default limits
- **THEN** the observation SHALL be clipped to 50,000 characters followed by a truncation marker stating the number of omitted characters

### Requirement: Trajectories are lossless and redacted
The system SHALL write trajectory events through a bounded asynchronous writer that applies backpressure to the turn (never drops events) when more than 10,000 events are pending, SHALL replace every value present in the secrets store and every match of the built-in credential patterns with `[REDACTED:<secret name or pattern id>]` before any byte reaches disk, and SHALL create trajectory files with mode 0600.
**Milestone:** M1

#### Scenario: Secret echoed by a tool
- **GIVEN** the secrets store holds `GITHUB_TOKEN`
- **WHEN** a shell command prints the token value
- **THEN** the stored `tool.result` SHALL contain `[REDACTED:GITHUB_TOKEN]` and not the value

### Requirement: Trajectory retention and size limits
The system SHALL compress a trajectory to `<session_id>.jsonl.zst` when its session ends, SHALL delete trajectories older than `observability.trajectories.retention_secs` (default 7776000; 0 keeps forever) unless the session is pinned, and SHALL stop recording payload bodies after `observability.trajectories.max_bytes_per_session` (default 256 MiB), emitting one `error` event with code `trajectory.size_limit` and continuing to record event headers. `observability.trajectories.enabled` (default true) SHALL only disable recording for sessions started after the change.
**Milestone:** M1

#### Scenario: Retention sweep
- **GIVEN** an unpinned session that ended 91 days ago
- **WHEN** the daily maintenance sweep runs
- **THEN** its trajectory file SHALL be deleted and the session row SHALL record `trajectory_purged_at`

### Requirement: Structured logs
The system SHALL write logs as JSON lines to `<profile home>/logs/prometheus.log` with the keys `ts`, `level`, `component`, `profile`, `session_id`, `turn_id`, `trace_id`, `msg` and `fields`; it SHALL rotate at 16 MiB keeping 5 files, create files with mode 0600, apply the same secret redaction as trajectories, and take the level filter from `logging.level` (default `info`) overridden only by `PMT_LOG` or `RUST_LOG`. Daemon-wide records with no profile (startup, update) SHALL go to `<PROMETHEUS_HOME>/logs/prometheus.log`. On an interactive terminal, WARN and above SHALL also be printed to stderr in human form.
**Milestone:** M1

#### Scenario: Turn context on every record
- **WHEN** a tool emits a log record during a turn
- **THEN** the record SHALL carry that turn's `session_id`, `turn_id` and `trace_id`

### Requirement: Log viewer
The log viewer `prometheus logs` owned by `cli` (`prometheus logs [<component>] [--follow] [--since S] [--level L] [--session|--root <id>]`, with the component list defined there) SHALL read the structured records this capability writes, print the absolute path it reads first and default to the last 50 records; an invalid `--since` value SHALL print `invalid --since value '<v>': use 30m, 2h, 1d or an RFC 3339 time` and exit 2, and an unknown component or level SHALL exit 2 naming the allowed values.
**Milestone:** M1

#### Scenario: Following a session's records
- **WHEN** the user runs `prometheus logs -f --session ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W`
- **THEN** the system SHALL print existing and new records whose `session_id` is `ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W` until interrupted, then exit 130

### Requirement: Crash and debug bundle (upgrade)
`prometheus debug bundle [--session ID] [--since 24h] [-o PATH]` SHALL write a local `tar.zst` containing the redacted logs for the window, the config with every secret reference replaced by its name, `prometheus doctor --json` output, `prometheus version --json`, crash reports, and (with `--session`) that session's trajectory; it SHALL print the path and SHALL never upload anything.
**Milestone:** M2

#### Scenario: Bundle contains no secret values
- **GIVEN** a profile whose secrets store holds three secrets
- **WHEN** the user creates a debug bundle
- **THEN** no file in the bundle SHALL contain any of the three secret values

### Requirement: Opt-in OpenTelemetry export
The system SHALL export no traces, metrics or logs unless `observability.otel.enabled = true` (default false); when enabled, `observability.otel.endpoint` SHALL be required (missing → config error, exit 78), `observability.otel.protocol` SHALL be `http/protobuf` (default) or `grpc`, request headers SHALL come only from secrets named in `observability.otel.header_secrets`, and `OTEL_*` environment variables SHALL NOT configure export (doctor SHALL warn when they are set). Export SHALL use a bounded queue of 4,096 items that drops oldest on overflow, counts drops in `prometheus.otel.dropped`, and never blocks a turn.
**Milestone:** M2

#### Scenario: Collector down
- **GIVEN** OTLP export enabled and the collector unreachable
- **WHEN** the agent runs a turn
- **THEN** the turn SHALL complete with no added latency above 5 ms and the dropped-item counter SHALL increase

### Requirement: Span model
When OpenTelemetry export is enabled the system SHALL emit spans named `session`, `turn`, `model.call`, `tool.call`, `subagent`, `compression`, `approval.wait`, `rpc.request`, `cron.run`, `eval.task` and `evolve.stage`, nested by causality, with GenAI semantic-convention attributes on `model.call` (`gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`) and `prometheus.*` attributes for profile, session, root session, purpose and cost. Message content, tool arguments and tool output SHALL NOT be attached unless `observability.otel.capture_content = true` (default false).
**Milestone:** M2

#### Scenario: Content not exported by default
- **GIVEN** OTLP export enabled with default settings
- **WHEN** a turn sends a prompt containing `confidential plan`
- **THEN** no exported span or event SHALL contain the string `confidential plan`

### Requirement: Trace context propagation
The system SHALL propagate W3C `traceparent` to subagents, WASM plugin calls, Python bridge calls and RPC requests issued by attached clients (which MAY send their own `traceparent` in request `meta`), and SHALL NOT send `traceparent` to model providers, MCP servers or other external hosts unless `observability.otel.propagate_external = true` (default false). The `trace_id` of a session SHALL equal the `trace_id` recorded in its log records and trajectory `session.start`.
**Milestone:** M2

#### Scenario: Subagent spans join the parent trace
- **WHEN** a turn delegates to two subagents
- **THEN** both subagents' `session` spans SHALL share the parent's `trace_id` and have the delegating `tool.call` span as parent

### Requirement: Metrics catalog
The system SHALL maintain these metrics, exported through OTLP when enabled and readable locally through `system.metrics` RPC: counters `prometheus.tokens` (attributes `direction` ∈ input|output|cache_read|cache_write|reasoning, `provider`, `model`, `purpose`), `prometheus.cost.usd`, `prometheus.model.calls` (`outcome`), `prometheus.tool.calls` (`tool`, `outcome`), `prometheus.turns`, `prometheus.errors` (`kind`), `prometheus.budget.exceeded` (`scope`), `prometheus.rpc.contract_violations`, `prometheus.otel.dropped`; histograms `prometheus.model.latency_ms`, `prometheus.model.ttft_ms`, `prometheus.tool.duration_ms`, `prometheus.turn.duration_ms`; gauges `prometheus.sessions.active`, `prometheus.subagents.active`. Metric attributes SHALL never include message content, file paths or user identifiers.
**Milestone:** M2

#### Scenario: Error counted by kind
- **WHEN** a provider call fails with an authentication error
- **THEN** `prometheus.errors` with `kind = auth` SHALL increase by 1

### Requirement: Local metrics scrape endpoint (upgrade)
When `observability.metrics_endpoint.enabled = true` (default false) the daemon SHALL serve the metrics catalog in Prometheus text exposition format at `GET /metrics` on the loopback health listener, without authentication and without any per-session or per-user labels.
**Milestone:** M2

#### Scenario: Scrape disabled by default
- **WHEN** a client requests `GET /metrics` on a default configuration
- **THEN** the daemon SHALL respond 404

### Requirement: Per-call usage ledger
The system SHALL append exactly one row to the `usage_ledger` table of the profile's `state.db` for every model call (main, auxiliary, title, compression, memory, subagent, cron, eval, harness improver, selector and reflector), with `call_id`, `ts`, `profile`, `session_id`, `root_session_id`, `turn_id`, `parent_call_id`, `purpose`, `provider`, `model`, `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_write_tokens`, `reasoning_tokens`, `cost_usd` (nullable), `cost_status` ∈ `exact|estimated|subscription|unknown` (as defined by `model-providers`), `billing_source` (`subscription:<provider>/<label>`, `api_key:<provider>/<label>` or `local`), `request_hash` and `cache_hit`; the table SHALL be append-only and keyed by `root_session_id` (design decision D17), and this SHALL be the only usage table; rows SHALL never be updated or deleted except by session deletion. A call served from the replay cache SHALL be recorded with `cache_hit = true` and `cost_usd = 0`.
**Milestone:** M1

#### Scenario: Auxiliary call is recorded
- **GIVEN** a turn that triggers a title-generation call on an auxiliary model
- **WHEN** the turn finishes
- **THEN** the ledger SHALL contain one row with `purpose = auxiliary.title` and the auxiliary model's name in addition to the main call's row

### Requirement: Totals keyed by root lineage
Every usage total the system reports (status bars, `/usage`, `prometheus usage`, dashboard, RPC `usage.*`) SHALL be computed from ledger rows grouped by `root_session_id`, so that compression continuations, resumed branches, subagents and background delegations count towards their root; for any filter, the reported total SHALL equal the sum of the matching ledger rows.
**Milestone:** M1

#### Scenario: No undercount across lineage
- **GIVEN** a root session that made 3 calls, was compressed into a continuation session that made 2 calls, and delegated to 2 subagents that made 4 calls each
- **WHEN** the user runs `prometheus usage --session <root id>`
- **THEN** the reported call count SHALL be 13 and the token totals SHALL equal the sum of those 13 ledger rows

### Requirement: Pricing and unpriced calls
The system SHALL price a call from the provider-reported cost when present, otherwise from the model metadata price table (per-million input, output, cache-read, cache-write and reasoning rates), and otherwise record `cost_usd = null` with `cost_status = unknown` (a subscription-covered call records `cost_status = subscription` as defined by `model-providers`). Unpriced calls SHALL be displayed as `—`, never as `$0.00`, and every total SHALL report `unpriced_calls` alongside `cost_usd`.
**Milestone:** M1

#### Scenario: Local model without a price
- **GIVEN** a call to a local model with no price entry
- **WHEN** the user runs `prometheus usage`
- **THEN** the totals SHALL show `unpriced_calls: 1` and SHALL NOT count that call as zero cost

### Requirement: Usage command
`prometheus usage [--since S] [--until S] [--by root|session|model|provider|purpose|day|profile] [--session ID] [--all-profiles] [--json]` SHALL print totals of calls, each token direction, `cost_usd` and `unpriced_calls` for the selection, defaulting to `--since 30d --by day` for the active profile; `--all-profiles` SHALL open each profile through its own ProfileScope and report per-profile and grand totals.
**Milestone:** M1

#### Scenario: Breakdown by purpose
- **WHEN** the user runs `prometheus usage --by purpose --since 7d`
- **THEN** the output SHALL list one row per purpose (for example `main`, `auxiliary.title`, `compression`, `subagent`) whose sum equals the overall total

### Requirement: Hard budgets
The system SHALL enforce, per profile, the optional budgets (the single `budgets.*` namespace, also used by `agent-turn-loop`) `budgets.turn_tokens`, `budgets.turn_usd`, `budgets.session_tokens`, `budgets.session_usd`, `budgets.root_usd`, `budgets.daily_usd`, `budgets.monthly_usd`, `budgets.root_tokens` and `budgets.unattended_root_usd` (default 5.00, applied to cron, unattended gateway sessions, background delegations and harness evolution); before each model call it SHALL estimate the worst-case cost (input tokens × input price + requested max output tokens × output price) and, when any applicable budget would be exceeded, SHALL NOT send the call, SHALL end the turn with an error of kind `budget_exceeded` naming the scope, limit and spend, emit `budget.exceeded`, and make a CLI one-shot exit 3. Days and months SHALL be evaluated in the profile's configured timezone. Unpriced-model rule: when a USD budget applies and the model has no known price, the call SHALL be refused before it is sent with error kind `budget_exceeded` and code `budget.unpriced_model`, unless `budgets.allow_unpriced = true` (default false), in which case only token budgets SHALL be enforced for that call and a WARN SHALL be logged once per session.
**Milestone:** M1

#### Scenario: Root budget stops a delegation tree
- **GIVEN** `budgets.root_usd = 1.00` and a root lineage that has spent 0.98 USD
- **WHEN** a subagent attempts a call estimated at 0.05 USD
- **THEN** the call SHALL NOT be sent, the subagent SHALL fail with `budget_exceeded` and the parent SHALL receive that failure as the delegation result

#### Scenario: Unpriced calls under a USD budget
- **GIVEN** a USD budget and a model with no price
- **WHEN** a call to that model is attempted
- **THEN** with `budgets.allow_unpriced = false` (the default) the call SHALL NOT be sent and SHALL fail with kind `budget_exceeded` and code `budget.unpriced_model`

### Requirement: Budget warnings and overrides
The system SHALL emit one `budget.warning` event and a user-visible notice per scope and period when spend first crosses `budgets.warn_ratio` (default 0.8) of a limit. Budgets SHALL be changeable only by the user (`/budget set <scope> <usd>`, `prometheus config set`), never by a model tool, and a raised budget SHALL take effect immediately because budgets are not part of the system prompt.
**Milestone:** M1

#### Scenario: Model cannot raise its own budget
- **WHEN** the agent attempts to write `budgets.root_usd` through any tool, including file tools targeting `config.toml`
- **THEN** the write SHALL be refused with kind `permission_denied`

### Requirement: Live usage events
After every model call the daemon SHALL emit a `usage.updated` RPC event to clients attached to the session with `{session_id, root_session_id, call: <ledger row without request_hash>, session_totals, root_totals, budgets: [{scope, limit, spent, remaining}]}`, so that CLI, TUI, desktop and dashboard display the same numbers.
**Milestone:** M2

#### Scenario: TUI and desktop agree
- **GIVEN** a TUI and a desktop window attached to the same session
- **WHEN** a model call completes
- **THEN** both SHALL receive the same `usage.updated` payload

### Requirement: Health endpoints
The daemon SHALL serve on a loopback-only listener (`observability.health.bind`, default `127.0.0.1:0` with the chosen port written to `gateway.json`) `GET /healthz` returning 200 `{"status":"ok"}` while its runtime responds within 1 s, and `GET /readyz` returning 200 when every profile's store is open and migrated and the scheduler is running, or 503 with `{"status":"not_ready","failing":[<component names>]}` otherwise. Neither endpoint SHALL require authentication or reveal profile names, session ids or secrets.
**Milestone:** M2

#### Scenario: Store migration in progress
- **GIVEN** the daemon is applying a schema migration to a profile's `state.db`
- **WHEN** a client requests `GET /readyz`
- **THEN** the daemon SHALL respond 503 with `failing` containing `store`

### Requirement: Component health model
Each daemon component (store per profile, scheduler, each adapter, each MCP server, each plugin host, the Python bridge, the OTLP exporter, each provider's circuit) SHALL report `ok`, `degraded` or `down` with a `reason` and `since`; every transition SHALL be logged at WARN (or INFO when returning to `ok`) and emitted as a `health.changed` event, and the authenticated RPC `system.health` SHALL return all component states for the connection's profile.
**Milestone:** M2

#### Scenario: Adapter loses its connection
- **WHEN** the Telegram adapter's long-poll fails 3 times in a row
- **THEN** its health SHALL become `degraded` with the last error as `reason`, and a `health.changed` event SHALL be emitted

### Requirement: Status command
`prometheus status [--json] [--check]` SHALL report whether the daemon runs (pid, uptime, version, build match), profiles served, active sessions and subagents, component health, the next scheduled job, and today's spend against `budgets.daily_usd`; without a daemon it SHALL report `daemon: not running` and exit 0. With `--check` it SHALL exit 1 when any component is `down` or the daemon is not running.
**Milestone:** M2

#### Scenario: Scripted health check
- **GIVEN** a daemon with one adapter `down`
- **WHEN** the user runs `prometheus status --check`
- **THEN** the system SHALL exit 1 and name the adapter

### Requirement: No outbound telemetry by default
The system SHALL send no usage statistics, crash reports, trajectories or metrics to any host other than the user's configured model providers, tools and explicitly configured OTLP endpoint; there SHALL be no vendor telemetry endpoint compiled into the binary.
**Milestone:** M1

#### Scenario: Fresh install network audit
- **GIVEN** a fresh install using only the mock provider
- **WHEN** the user runs a chat session with network egress recorded
- **THEN** no outbound connection SHALL be observed

### Requirement: Observability overhead budget
With OpenTelemetry export disabled, trajectory recording and logging SHALL add at most 3% to the wall time of the offline smoke eval suite compared with recording disabled, and with export enabled at most 5%; CI SHALL measure both on the reference runner and fail on violation.
**Milestone:** M2

#### Scenario: Overhead gate
- **WHEN** a change makes trajectory writing synchronous on the turn path
- **THEN** the overhead gate SHALL fail if smoke-suite wall time rises by more than 3%

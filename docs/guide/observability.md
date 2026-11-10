# Logs, trajectories and telemetry

Prometheus records what it does in two places: **structured logs** (what
the process did: warnings, errors, timings) and **trajectories** (what a
session did: every message, model call, tool call and result, in one
JSON-lines grammar shared by every surface).

Every command below is run by the documentation tests against a fresh home,
in order. Lines starting with `$` are commands; the lines after a command
are (part of) its output, with `<...>` standing for text that varies.

## Structured logs

Every log record is one JSON line with the keys `ts`, `level`,
`component`, `profile`, `session_id`, `turn_id`, `trace_id`, `msg` and
`fields`. Records emitted inside a turn carry that turn's session, turn and
trace ids, so one session's activity can be pulled out of a busy log.

Records go to:

| Records | File |
|---|---|
| a profile's activity | `<profile>/logs/prometheus.log` |
| the `gateway`, `update` and `migrate` components | `<home>/logs/<component>.log` |
| process-wide records with no profile (startup) | `<home>/logs/prometheus.log` |

Files are created with mode `0600`, rotate when they reach 10 MiB and keep
5 files in all (`prometheus.log`, `prometheus.log.1` ... `prometheus.log.4`).
Secret values from the profile's secret store and the launch environment
are replaced by `[REDACTED:<NAME>]`, and credential-shaped strings by
`[REDACTED]`, before anything is written. On an interactive terminal,
warnings and errors are also printed to stderr.

The level comes from `logging.level` in `config.toml` (default `info`);
`PMT_LOG` (or else `RUST_LOG`) overrides it for one run and accepts
directives such as `PMT_LOG=info,pmt_store=debug`.

## Reading logs

`prometheus logs [<component>]` prints the last 50 records of a component
(`agent` by default), after a header naming every file it read:

```console
$ prometheus logs update
==> <home>/logs/update.log <==
$ prometheus logs list
agent      <home>/profiles/default/logs/prometheus.log
errors     <home>/profiles/default/logs/prometheus.log
gateway    <home>/logs/gateway.log
```

A gateway daemon writes its own records (startup, connections, the idle
session reaper) as the `gateway` component; the work on a session it
hosts is recorded in that session's profile, so with `gateway.multiplex`
each served profile's sessions and startup warnings are in its own
`logs/`.

`errors` selects warnings and errors of every component. The filters
combine:

- `-n/--lines N`: the last N matching records (`0` for all); older rotated
  files are read, and named in the header, only when needed;
- `--level L`: `error`, `warn`, `info`, `debug` or `trace` and more severe;
- `--since S`: `30m`, `2h`, `1d` ago, or an RFC 3339 time;
- `--session <id>`: one session; `--root <id>`: every session of a
  lineage (compression continuations, forks, delegated children);
- `-f/--follow`: keep printing new matching records until Ctrl-C, which
  exits 130;
- `--json`: one `prometheus.logs.v1` document (`files`, `records`); with
  `--follow`, a header line and then one record per line.

An invalid `--since`, an unknown component or an unknown level exits 2:

```console
$ prometheus logs --since yesterday # exits 2
error: invalid --since value 'yesterday': use 30m, 2h, 1d or an RFC 3339 time
```

## Trajectories

Each session is recorded at `<profile>/trajectories/<session_id>.jsonl`
while it runs and compressed to `<session_id>.jsonl.zst` when it ends.
Every line has the keys `v` (grammar version), `ts`, `seq`, `session_id`,
`root_session_id`, `turn_id`, `span_id`, `parent_span_id`, `type` and
`payload`; the event types and their payloads are defined in
[`schema/trajectory/v1.json`](https://github.com/CyberdyneCorp/prometheus-agent/blob/main/schema/trajectory/v1.json).

- Recording never drops an event: when 10,000 events are waiting to be
  written, the session waits.
- Secrets are redacted like in logs, and files are `0600`.
- Past `observability.trajectories.max_bytes_per_session` (256 MiB) one
  `error` event with code `trajectory.size_limit` is written and later
  events keep their header but not their payload.
- The daily maintenance deletes the trajectories of unpinned sessions that
  ended more than `observability.trajectories.retention_secs` ago (90 days;
  `0` keeps them), and those of deleted sessions.
- `observability.trajectories.enabled = false` stops recording for sessions
  started after the change.

`prometheus trajectory show <session>` renders a trajectory as canonical
text, the same text harness evolution and eval read:

```text
[USER]
Summarise notes.txt and list the TODOs in src/.

[ACTION] read_file({"path":"notes.txt"})

[OBSERVATION ERROR]
not_found: no such file: notes.txt
```

Tool arguments are clipped at 2,000 characters (`--args-trunc`), each
observation at 50,000 (`--obs-trunc`, never below 10,000) and the whole
output at `--max-chars`, with the marker `…(+N chars — render truncation)`.
`--format jsonl` prints the stored lines and `--json` one
`prometheus.trajectory.v1` document. A trajectory written by a newer minor
grammar renders its unknown events as `[EVENT <type>]`; a newer major
version exits 76 asking you to run `prometheus update`. A session without a
trajectory exits 66:

```console
$ prometheus trajectory show ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W # exits 66
error: session ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W has no trajectory in <home>/profiles/default/trajectories
```

## Daemon status and health

`prometheus status` says whether the home's daemon runs and how it is:
pid, uptime, version and whether it runs this build, the served profiles,
active sessions, running turns and subagents, every component's health,
the next scheduled job and today's spend against `budgets.daily_usd`.
Without a daemon it says so and exits 0:

```console
$ prometheus status
daemon: not running
next job: none scheduled
spend today: $0.0000 no daily budget, 0 unpriced calls
$ prometheus status --check # exits 1
daemon: not running
```

- `--check` exits 1 when the daemon is not running or any component is
  `down` (the report names it), for scripts and health checks.
- `--liveness` asks the daemon's `/healthz`: exit 0 on 200 printing
  nothing, 1 otherwise.
- `--full` adds the active profile's model and provider, the providers
  whose credential is present (names only), the adapters the daemon
  would start for each served profile, every scheduled job's next run,
  the terminal backend and sandbox enforcement, and session counts
  (active, today, total). It makes no outbound connection.
- `--full --deep` also probes each provider with a credential (one
  authenticated model-listing request), each enabled adapter's platform
  and each MCP server (`initialize`), 10 s each, and reports latency or
  the error. It is the only status mode that reaches the network.
- `--exports` reports every telemetry exporter (OTLP traces, metrics and
  logs, the local scrape endpoint, the gateway health export, NeMo Relay
  and Langfuse): whether it is enabled, its endpoint host (never
  credentials), the last successful export and queued and dropped items,
  and what can leave the host (`capture_content`, `propagate_external`).
- `--json` prints one `prometheus.status.v1` document
  (`prometheus.status_exports.v1` with `--exports`).

```console
$ prometheus status --exports
no telemetry leaves this host
```

Every daemon component reports `ok`, `degraded` or `down` with a reason
and the time it entered that state: each profile's `store`, the
`scheduler`, each `adapter:<platform>` (degraded after 3 failed
connections or polls in a row, down once it failed for good), each
`mcp:<server>`, each `python_bridge:<plugin>`, the `plugins` host, the
`otel` exporter and each `provider:<id>` circuit (degraded while its
health score is below `fallback.min_health`). Every transition is logged
(WARN, INFO back to `ok`) and sent to attached clients as
`health.changed`; `system.health` returns them over the client protocol.

The daemon also serves a loopback-only health listener
(`observability.health.bind`, default `127.0.0.1:0`; its URL is
`health_url` in `<home>/run/gateway.json`), without authentication and
without profile names, session ids or secrets:

| Path | Answer |
|---|---|
| `GET /healthz` | 200 `{"status":"ok"}` while the runtime responds within 1 s |
| `GET /readyz` | 200 when every store is open and migrated and the scheduler runs; else 503 `{"status":"not_ready","failing":["store"]}` |
| `GET /metrics` | the metrics catalog in Prometheus text format with `observability.metrics_endpoint.enabled = true`; 404 otherwise |

After every model call the daemon sends `usage.updated` to the clients
attached to the session: the call's ledger row (without its request
hash), the session's and the lineage's totals and every budget with its
spend, so CLI, TUI, desktop and dashboard show the same numbers.

## Metrics

One catalog of metrics is kept per process, readable through
`system.metrics`, the scrape endpoint and OTLP export. Attributes are a
closed set of bounded values (direction, provider, model, purpose,
outcome, tool, kind, scope): never message content, file paths or user
identifiers.

| Metric | Kind | Attributes |
|---|---|---|
| `prometheus.tokens` | counter | `direction` (input, output, cache_read, cache_write, reasoning), `provider`, `model`, `purpose` |
| `prometheus.cost.usd` | counter | `provider`, `model`, `purpose` |
| `prometheus.model.calls` | counter | `provider`, `model`, `outcome` |
| `prometheus.tool.calls` | counter | `tool`, `outcome` |
| `prometheus.turns` | counter | `outcome` (the exit reason) |
| `prometheus.errors` | counter | `kind` (`auth`, `rate_limit`, a tool failure's kind...) |
| `prometheus.budget.exceeded` | counter | `scope` |
| `prometheus.rpc.contract_violations` | counter | |
| `prometheus.otel.dropped` | counter | |
| `prometheus.model.latency_ms`, `prometheus.model.ttft_ms`, `prometheus.tool.duration_ms`, `prometheus.turn.duration_ms` | histogram | |
| `prometheus.sessions.active`, `prometheus.subagents.active` | gauge | |

## OpenTelemetry export

Nothing is exported unless the profile that runs the work (the daemon's
launch profile) enables it:

```toml
[observability.otel]
enabled = true
endpoint = "http://127.0.0.1:4318"   # required; https://, or http:// to loopback
protocol = "http/protobuf"            # or "grpc" (e.g. http://127.0.0.1:4317)
header_secrets = ["OTLP_API_KEY"]     # sent as the header otlp-api-key
capture_content = false
propagate_external = false
```

Enabled without an endpoint is a configuration error (exit 78). Request
headers come only from the named secrets, and `OTEL_*` environment
variables configure nothing (`prometheus doctor` warns when they are set).
Items wait in a queue of 4,096 that drops the oldest when full; a failed
export drops its items too, counting them in `prometheus.otel.dropped`, so
an unreachable collector never slows a turn. Metrics are posted every 15
seconds as cumulative sums and histograms; log records are never exported.

Spans follow the work: `session` > `turn` > `model.call` and `tool.call`
(> `approval.wait`), with `subagent` and `compression` under the turn,
plus `cron.run` around a scheduled run's session, `rpc.request` around
each client request, and `eval.task` / `evolve.stage` from harness
evolution. `model.call` carries the GenAI attributes `gen_ai.system`,
`gen_ai.request.model`, `gen_ai.usage.input_tokens` and
`gen_ai.usage.output_tokens`; every span carries `prometheus.profile`,
`prometheus.session_id` and `prometheus.root_session_id`, and calls
their `prometheus.purpose` and `prometheus.cost_usd`. Message content,
tool arguments and tool output are attached only with
`capture_content = true`.

Every session of one lineage (compression continuations, forks,
subagents) shares one trace: its id is the `trace_id` of the session's
log records and of its trajectory's `session.start`, and a subagent's
`session` span hangs under the delegating
[`delegate_task`](../reference/tools.md#delegate_task) tool call. The
W3C `traceparent` of the current span goes to WASM plugin hooks and
Python-bridge calls (a `traceparent` field), and a client may send its
own in a request's `meta.traceparent`; it is sent to model providers only
with `propagate_external = true`.

## Liveness watchdog and task dumps

A thread outside the daemon's async runtime probes it every
`daemon.watchdog.probe_interval_secs` (30, in `daemon.toml`). After
`daemon.watchdog.max_strikes` (3) probes in a row that did not run within
the interval, it writes a task dump to
`<home>/logs/task-dump-<UTC timestamp>.txt`, marks `run/gateway.json`
`state = "degraded"` with `exit_reason = "runtime_liveness_watchdog"` and
exits 75 so the service supervisor restarts the daemon; `prometheus
status` shows that record (`last daemon: degraded
(runtime_liveness_watchdog) at ...`) until a new daemon replaces it.
`daemon.watchdog.enabled = false` turns it off. While the runtime runs the
daemon refreshes `updated_at` in `run/gateway.json` every minute, and
`status` warns `daemon heartbeat stale (<n>s)` once it is older than two
minutes with the process alive.

A task dump lists the runtime's counters (workers, tasks alive, queued),
every thread that is inside a span with where it is, and every open span
(sessions, turns, client requests, scheduled runs...) as a tree with its
age. `prometheus debug dump-tasks` (every platform, over the control
channel) or SIGUSR2 (POSIX) writes one without stopping the daemon:

```console
$ prometheus debug dump-tasks # exits 69
error: the daemon is not running; there are no tasks to dump
```

With `daemon.systemd_watchdog_secs > 0` (Linux), `prometheus daemon
install-service` writes a `Type=notify` unit with that `WatchdogSec`; the
daemon reports ready once it serves and sends a heartbeat only after each
answered probe.

## Usage insights

`prometheus usage insights [--days N] [--source S] [--all-profiles]
[--json]` (default the last 30 days) and `/insights [days]` in the CLI,
TUI, desktop and dashboard report what the profile did, from the usage
ledger and the session store and without any model call: sessions, user
messages, turns, tool calls, active days and tokens per direction, the
cost with `unpriced_calls`, calls by model, by provider and by source
(the surface or messaging platform a lineage started from), the ten most
called tools with their success rate, skill loads, turns by weekday and
hour in the profile's time zone, and the five costliest root lineages by
cost and by tokens. Every session counts towards its lineage's root, so a
Telegram conversation's subagents are Telegram usage. `--source telegram`
keeps those lineages only; a source with no activity in the window exits 2
listing the sources present.

```console
$ prometheus usage insights --days 7
Insights for the last 7 days
sessions 0, user messages 0, turns 0, tool calls 0, active days 0
```

## Debug bundles and shared reports

`prometheus debug bundle [--session ID] [--since 24h] [-o PATH]` writes a
local `tar.zst` for a bug report: the log records of the window, the
profile's `config.toml` and the home's `daemon.toml`, `prometheus doctor
--json`, `prometheus version --json`, crash reports and, with
`--session`, that session's trajectory. Every member is redacted with the
profile's known secret values and credential patterns (whatever
`security.redact_secrets` says) and URL credentials become `***`. It
prints the path and uploads nothing.

`prometheus debug share [--lines N] [--expire-days D] [--target
paste_rs|dpaste|custom] [--no-redact] [--yes]` builds a report from
`debug summary` plus the last `--lines` (200) log records of the daemon
and the active profile, redacted like trajectories, shows it (or its path
when it is longer than a screen) and asks `Upload this report to <host>?
It will be public to anyone with the link. [y/N]`. Only `y` uploads;
anything else makes no connection and exits 1. Without a terminal it
needs `--yes`, else it uploads nothing and exits 4; `--no-redact` asks a
second time and never runs unattended.

```console
$ prometheus debug share # exits 4
debug share asks before uploading: run it on a terminal or pass --yes; nothing was uploaded
```

- `paste_rs` (default) posts to paste.rs, which can delete; `dpaste`
  posts to dpaste.com with `--expire-days` (7); `custom` posts JSON
  `{"content", "expire_days"}` to `debug.share.endpoint` (`https://`, or
  `http://` to a loopback host) with headers from the secrets named in
  `debug.share.header_secrets`, and reads the URL (and an optional
  `delete_token`) from its answer.
- Every upload is recorded in `<home>/logs/shares.json`;
  `prometheus debug delete <url>` deletes a recorded share where the
  target supports it.
- `--local` writes the same report to `<home>/logs/debug-report-<UTC>.txt`
  without any network access.
- `/debug` in the CLI, TUI and desktop writes the report locally;
  `/debug share` shows it and asks, and only `/debug share yes` uploads
  that same report to paste.rs.

## ShareGPT export

`prometheus trajectory export --format sharegpt|jsonl [--session ID ... |
--eval-run ID | --since S] [--include-failed] -o FILE` writes one JSON
line per session (every session by default), redacted like trajectories.
`sharegpt` records hold `conversations` (`from` is `system`, `human`,
`gpt` or `tool`), the session's tool definitions under `tools` and
`meta` (`session_id`, `model`, `completed`, `toolsets`). Assistant tool
calls become `<tool_call>{"name": ..., "arguments": ...}</tool_call>`
blocks and reasoning a leading `<think>…</think>` in the `gpt` value;
tool results become `<tool_response>{"tool_call_id": ..., "name": ...,
"content": ...}</tool_response>` in a `tool` turn. Only the session's
frozen system prompt is kept (no ephemeral overlays or grader material).
`jsonl` records hold the session's events instead. Sessions that ended in
an error are left out unless `--include-failed`; the file loads with any
JSON-lines dataset loader.

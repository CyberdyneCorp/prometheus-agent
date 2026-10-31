# Logs and trajectories

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

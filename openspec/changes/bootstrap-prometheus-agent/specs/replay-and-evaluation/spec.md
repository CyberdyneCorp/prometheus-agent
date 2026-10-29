# Replay and Evaluation

## Purpose
Makes Prometheus' agent behaviour reproducible and measurable: every model call is content-addressed and optionally cached, sessions can be exported as self-contained recordings and replayed offline with divergence detection, a deterministic mock provider and an offline demo exercise the whole pipeline without credentials, and `prometheus eval` runs task suites with strictly post-hoc judging. The key decision is the answer-free boundary: grader material and scores are reachable only by the judge phase, never by the agent, harness evolution or selection.

## ADDED Requirements

### Requirement: Canonical request hash
The system SHALL compute for every model call a `request_hash` of the form `sha256:<64 hex>` over the canonical JSON (keys sorted, no insignificant whitespace, numbers in shortest round-trip form) of exactly: provider id, wire API, model, system prompt, messages, tool definitions, tool choice, sampling parameters (temperature, top_p, max output tokens, stop, seed, reasoning effort) and response format. Transport-only fields (request ids, timestamps, auth headers, user agent, streaming flag, prompt-cache markers) SHALL be excluded, so that a streamed and a non-streamed form of the same request hash identically.
**Milestone:** M2

#### Scenario: Cache markers do not change the hash
- **GIVEN** two otherwise identical requests, one with prompt-cache breakpoints and one without
- **WHEN** their hashes are computed
- **THEN** the two `request_hash` values SHALL be equal

### Requirement: Content-addressed model-call cache
The system SHALL provide a model-call cache controlled by `replay.cache.mode` ∈ `off|read|write|read_write` (default `off` for interactive sessions; eval runs, harness evolution and the demo SHALL default to `read_write`), storing each response as the normalised event stream plus usage at `<profile home>/cache/model-calls/<first 2 hex>/<hash>.json.zst` with mode 0600, namespaced by `replay.cache.namespace` (default `default`). A hit SHALL replay the stored stream with the recorded chunk boundaries and SHALL be recorded in the usage ledger with `cache_hit = true` and `cost_usd = 0`.
**Milestone:** M2

#### Scenario: Resumed evaluation makes no new calls
- **GIVEN** an eval run that completed with `read_write` caching
- **WHEN** the same run is executed again with the same suite, model and harness
- **THEN** zero requests SHALL reach the provider and every ledger row for the rerun SHALL have `cache_hit = true`

### Requirement: Cache administration and limits
`prometheus cache stats|prune [--older-than 30d] [--namespace NS]|clear [--namespace NS]` SHALL report and manage the model-call cache, and the cache SHALL evict least-recently-used entries when it exceeds `replay.cache.max_bytes` (default 4 GiB). Cache entries SHALL never contain credentials, and entries whose response was an error SHALL never be stored.
**Milestone:** M2

#### Scenario: Errors are not cached
- **GIVEN** cache mode `read_write`
- **WHEN** a provider answers a request with HTTP 529
- **THEN** no cache entry SHALL be written for that request hash

### Requirement: Session recordings
`prometheus replay export <session_id> [-o FILE] [--include-workspace] [--lineage]` SHALL write a `.pmtrec` file (zstd-compressed tar) with a `manifest.json` (`format_version`, `prometheus_version`, `trajectory_grammar`, `session_ids`, `created_at`, `sha256` of every member), the trajectory, every referenced model response keyed by `request_hash`, every tool result, and the initial-state snapshot (config with secrets replaced by their names, system-prompt layers, enabled toolsets, skill and memory snapshot hashes, harness version, and a workspace manifest of path, size and sha256). File contents SHALL be included only with `--include-workspace`, and the exporter SHALL apply trajectory redaction to every member.
**Milestone:** M2

#### Scenario: Recording without workspace contents
- **WHEN** the user exports a session without `--include-workspace`
- **THEN** the recording SHALL list workspace files with hashes and SHALL contain none of their contents

### Requirement: Replay modes
`prometheus replay run <recording> [--mode strict|tools-live|model-live] [--on-divergence stop|continue] [--out DIR]` SHALL re-execute the recorded session and write a new trajectory: `strict` (default) serves model responses and tool results from the recording; `tools-live` serves model responses from the recording and executes tools for real inside a scratch copy of the recorded workspace (requires `--include-workspace` recordings); `model-live` replays user inputs and tool results against the configured live provider. The command SHALL exit 0 when the replay completed without divergence, 1 on divergence, and 65 for a malformed or tampered recording.
**Milestone:** M2

#### Scenario: Tampered recording
- **GIVEN** a recording whose trajectory member no longer matches its manifest hash
- **WHEN** the user runs `prometheus replay run` on it
- **THEN** the system SHALL refuse with `recording integrity check failed: <member>` and exit 65

### Requirement: Divergence detection
During replay the system SHALL compare each new model request's `request_hash` and each tool call's name and canonical arguments with the recording at the same step; at the first mismatch it SHALL report `divergence at step <n> (<model.call|tool.call>)` with a unified diff of the canonical forms truncated to 200 lines, and then stop or (with `--on-divergence continue`) fall through to the live provider and live tools for the remainder. A strict replay of an unchanged build SHALL produce a trajectory identical to the original after removing ids and timestamps.
**Milestone:** M2

#### Scenario: Prompt change detected
- **GIVEN** a recording made before a change to the system prompt's tool-guidance layer
- **WHEN** it is replayed in `strict` mode on the changed build
- **THEN** the system SHALL report divergence at the first model call with a diff showing the changed system prompt lines

### Requirement: Replay has no side effects
In `strict` mode the system SHALL run the replayed session with an empty capability grant: no network egress, no writes outside the replay output directory, no message delivery through any adapter, no cron or kanban mutations, no memory or skill writes, and no secrets; a tool call that has no recorded result SHALL be a divergence, never a live execution.
**Milestone:** M2

#### Scenario: Recorded message send is not re-sent
- **GIVEN** a recording in which the agent called `send_message` to a Telegram chat
- **WHEN** it is replayed in `strict` mode
- **THEN** the recorded tool result SHALL be returned and no message SHALL be sent

### Requirement: Golden recording regression gate
The repository SHALL keep golden recordings under `tests/golden/*.pmtrec` covering at least: a plain chat, a multi-tool turn, an approval denial, a compression event, a delegation to two subagents, and a budget refusal; CI SHALL replay each in `strict` mode on every change and fail on any divergence. `prometheus replay bless <recording> [--mode strict]` SHALL re-record a golden after an intended change and print the diff it accepts.
**Milestone:** M2

#### Scenario: Unintended prompt drift
- **WHEN** a change reorders two system-prompt layers without blessing goldens
- **THEN** CI SHALL fail naming each diverging golden recording

### Requirement: Deterministic mock provider
The system SHALL ship a built-in provider `mock` that needs no credentials or network and is always available, whose behaviour comes from a script file (`--mock-script PATH` or `providers.mock.script`) of ordered rules matching on step index, the last user message (substring or regex) or the last tool result, and responding with text, reasoning, one or more tool calls, or an error. Without a script it SHALL echo the last user message prefixed with `mock: `. Token usage SHALL be computed as ceil(UTF-8 bytes / 4) per message, cost SHALL be 0, and two runs with the same script and inputs SHALL produce byte-identical streams.
**Milestone:** M1

#### Scenario: Scripted tool call
- **GIVEN** a mock script whose first rule responds with a `read_file` call for `README.md` and whose second rule responds with text `done`
- **WHEN** the user runs `prometheus chat -z "summarise" --provider mock --mock-script s.toml`
- **THEN** the agent SHALL execute `read_file` once, print `done`, and exit 0

### Requirement: Mock fault injection
Mock script rules SHALL be able to inject the faults `rate_limit` (with `retry_after_secs`), `timeout`, `server_error` (HTTP status), `auth_error`, `context_overflow`, `malformed_stream` and `truncated_stream`, and a chunk delay `latency_ms` (default 0), so that retries, fallback chains, compression triggers and error handling are testable offline.
**Milestone:** M1

#### Scenario: Fallback on rate limit
- **GIVEN** a fallback chain `mock:primary` → `mock:secondary` where the primary script injects `rate_limit` on every call
- **WHEN** a turn runs
- **THEN** the turn SHALL complete using `mock:secondary` and the trajectory SHALL record the fallback

### Requirement: Offline smoke demo
`prometheus demo [--output DIR] [--keep]` SHALL run, with the mock provider and no network, a bundled scenario in a temporary profile under `DIR` (default a fresh temporary directory) in which the agent inspects a small ledger repository, runs its script through the terminal tool, finds the sign bug, edits the file, re-runs the script, and reports `Total: 85.15`; it SHALL print each step in the canonical trajectory rendering, finish within 30 s on the reference runner, never touch the user's profiles, and exit 0 only when the final answer matches.
**Milestone:** M1

#### Scenario: First run on a fresh machine
- **GIVEN** a fresh install with no providers configured
- **WHEN** the user runs `prometheus demo`
- **THEN** the system SHALL complete the scenario, print `demo passed`, and exit 0

### Requirement: Offline self-improvement demo
`prometheus demo --evolve [--k 2] [--mode parallel|sequential|both]` SHALL run the bundled two-task `accounting` domain through harness evolution (propose, probe, compose, blind selection, reflection, experience-library write) with a deterministic mock proposer and the mock provider, write `<output>/demo.json` with `tasks_completed`, `tasks_failed`, `executions` and `editor_calls`, print `demo: <done> tasks completed; <failed> failed`, exit 1 if any task failed, and on re-run resume without new executions or editor calls.
**Milestone:** M3

#### Scenario: Resume makes no new work
- **GIVEN** a completed `prometheus demo --evolve --output d`
- **WHEN** the same command is run again
- **THEN** `demo.json` SHALL report zero new executions and zero new editor calls

### Requirement: Eval suite format
An eval suite SHALL be a directory containing `suite.toml` (`name`, `version`, `domain`, `default_timeout_secs` default 600, `default_toolsets`, optional `budget_usd`) and `tasks/<task_id>/` each with `task.toml` (`prompt`, optional `domain`, `timeout_secs`, `toolsets`, `capabilities`, `files` copied into the task workspace) and an optional `grader/` directory; `prometheus eval validate <suite>` SHALL check the format and exit 65 naming the first invalid file. Suites SHALL be discovered from `<profile home>/evals/` and paths given on the command line.
**Milestone:** M3

#### Scenario: Missing prompt
- **GIVEN** a task directory whose `task.toml` lacks `prompt`
- **WHEN** the user runs `prometheus eval validate ./suite`
- **THEN** the system SHALL exit 65 naming `tasks/<id>/task.toml` and the missing key

### Requirement: Eval run
`prometheus eval run <suite> [--provider P] [--model M] [--harness REF] [--tasks a,b] [--repeat N] [--parallel N] [--cache off|read|read_write] [--budget-usd X] [--run-id ID]` SHALL execute every selected task in a fresh isolated session with its own workspace copy, under `--parallel` concurrent tasks (default 4), writing `<profile home>/evals/runs/<run_id>/` with `run.json` (suite version, provider, model, harness ref and hash, prometheus build, cache mode, started/finished), and per task and repeat `trajectory.jsonl`, `deliverables/` and `result.json` (`ok`, `delivered`, `cost_usd`, `wall_secs`, `error`) containing no score. The command SHALL exit 0 when every task executed, 1 when any task failed to execute, and 3 when the budget stopped the run.
**Milestone:** M3

#### Scenario: Budget stops a run
- **GIVEN** `--budget-usd 0.50` and a suite whose tasks cost 0.20 USD each
- **WHEN** the run reaches the third task
- **THEN** the third task's model call SHALL be refused, remaining tasks SHALL be marked `skipped: budget`, and the command SHALL exit 3

### Requirement: Eval runs are resumable
Re-running `prometheus eval run` with an existing `--run-id` SHALL skip tasks whose completion marker records an output fingerprint matching the current deliverables, SHALL re-execute (after deleting stale outputs and printing the reason) tasks whose marker is missing or whose deliverables were modified, and SHALL refuse with exit 2 if the suite version, model or harness hash differ from `run.json`.
**Milestone:** M3

#### Scenario: Tampered deliverable
- **GIVEN** a completed task whose deliverable was edited by hand
- **WHEN** the run is resumed
- **THEN** the system SHALL print `re-running <task>: deliverables changed since completion` and execute it again

### Requirement: Answer-free boundary
The system SHALL keep every task's `grader/` directory, reference answers, rubrics and scores outside the capability grant of the task session, every harness-evolution improver, selector and reflector session, and every experience-library write; a task whose trajectory shows an attempt to read a path inside a `grader/` directory or matching the suite's `leak_globs` SHALL be marked `contaminated` and excluded from scoring. Run results passed to harness evolution or selection SHALL contain only `ok`, `delivered`, `cost_usd`, `wall_secs` and `error`.
**Milestone:** M3

#### Scenario: Agent searches for the answer
- **WHEN** a task session runs `find / -name "reference_answer*"`
- **THEN** the grader files SHALL not be visible to it, and if any matching path is accessed the task SHALL be marked `contaminated`

### Requirement: Post-hoc judging
`prometheus eval judge <run_id> [--parallel 8]` SHALL be the only code path that reads grader material, SHALL score each executed task with its grader type — `exact` (normalised string equality), `regex`, `script` (a sandboxed command given the deliverables that prints `{"n_passed", "n_criteria"}`) or `llm` (a judge model with the rubric, recorded with `purpose = eval.judge`) — and SHALL write `judged.json` rows `{task_id, repeat, n_passed, n_criteria, all_pass, frac, graded, error}`. A grader failure or zero criteria SHALL produce an ungraded row (`frac = null`), never a zero; tasks that did not execute or are contaminated SHALL be ungraded without invoking the grader. The command SHALL exit 0 when every row is graded and 5 (partial result) otherwise.
**Milestone:** M3

#### Scenario: Judge failure is not a zero
- **GIVEN** an `llm` grader whose judge call times out for one task
- **WHEN** judging completes
- **THEN** that row SHALL have `graded: false` and an `error`, it SHALL be excluded from score denominators, and the command SHALL exit 5

### Requirement: Re-judging and judge identity
Re-running `prometheus eval judge` SHALL merge rows by `(task_id, repeat)`, SHALL keep an existing graded row when the new attempt is ungraded, and SHALL refuse with exit 1 and `judge changed for this run (<old> → <new>); use --judge-namespace to score separately` when the judge model or grader version differs from the one recorded in `judged.json`, unless `--judge-namespace NAME` writes to a separate `judged.<NAME>.json`.
**Milestone:** M3

#### Scenario: Flaky re-judge keeps the grade
- **GIVEN** a graded row for task `t1`
- **WHEN** a re-judge of `t1` fails
- **THEN** the graded row SHALL be kept unchanged

### Requirement: Eval reports and comparison (upgrade)
`prometheus eval report <run_id>... [--format text|markdown|json]` SHALL print per run `score` (mean `frac` × 100, two decimals), `all_pass` percentage, `n` graded, `n_ungraded`, `contaminated`, total cost, unpriced calls, total tokens and wall time; with two or more runs it SHALL add paired per-task deltas against the first run and a 95% bootstrap confidence interval of the score difference (10,000 resamples, seed 0), and SHALL print `NOTE: difference within noise` when the interval contains 0. With `--repeat N > 1` it SHALL also report pass@1, pass@N and the per-task standard deviation.
**Milestone:** M3

#### Scenario: Harness comparison
- **GIVEN** two judged runs of the same suite with harness A and harness B
- **WHEN** the user runs `prometheus eval report runA runB`
- **THEN** the report SHALL show the score delta B − A with its 95% confidence interval

### Requirement: Benchmark adapter protocol
External benchmarks SHALL plug in as adapters (WASM plugin, Python-bridge plugin, or subprocess speaking JSON-RPC over stdio) that implement `bench.describe` (name, version, domains, `leak_globs`), `bench.tasks {split}` (stable order), `bench.task {task_id}` (prompt with grading criteria removed, domain, files, timeout), `bench.prepare {task_id, workspace}`, `bench.deliverables {task_id, run_dir}`, `bench.preflight` and `bench.score {task_id, run_dir}`. The host SHALL call `bench.score` only from `prometheus eval judge`, and `bench.score` returning null or failing SHALL produce an ungraded row. `prometheus eval run bench:<name>[:<split>]` SHALL run an adapter's tasks with the same run layout as a suite.
**Milestone:** M3

#### Scenario: Score not callable during execution
- **WHEN** `prometheus eval run bench:example` executes tasks
- **THEN** the host SHALL NOT invoke `bench.score`, and an adapter attempt to push scores during execution SHALL be rejected with `-32002`

### Requirement: Built-in smoke suite
The binary SHALL bundle an eval suite named `smoke` of at least 12 offline tasks using the mock provider (tool calling, file edit, multi-step terminal use, approval denial, clarify, delegation, compression, skill invocation, memory write, budget refusal, interrupt, error recovery), each with an `exact` or `script` grader (tasks whose capability is not yet in the build are skipped and listed), run in CI as offline `prometheus demo`-style assertions with the mock provider so that it SHALL pass on every Tier 1 and Tier 2 target from M1; once `prometheus eval` ships, `prometheus eval run smoke && prometheus eval judge <run>` SHALL score the same suite at 100%.
**Milestone:** M1

#### Scenario: Regression in tool dispatch
- **WHEN** a change breaks parallel tool-call dispatch
- **THEN** the smoke suite SHALL score below 100% and CI SHALL fail

### Requirement: Session-to-task capture (upgrade)
`prometheus eval capture <session_id> --suite <dir> [--task-id ID]` SHALL turn a real session into a regression task: the first user message becomes the prompt, the recorded workspace manifest (and contents when available) becomes the task files, and the user SHALL supply the grader (`--expect TEXT`, `--expect-regex RE` or `--grader-script PATH`); the command SHALL refuse with exit 65 when the session's recording has no workspace contents and the task declares files.
**Milestone:** M3

#### Scenario: Bug report becomes a regression task
- **GIVEN** a session in which the agent mis-summed a ledger
- **WHEN** the user runs `prometheus eval capture ses_01JB7Z3Q5X8V2K4M6N8P0R2T42 --suite ./evals/accounting --expect "Total: 85.15"`
- **THEN** a new task directory SHALL be created with the session's prompt and an `exact` grader expecting `Total: 85.15`

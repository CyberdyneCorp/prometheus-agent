# Evaluation suites

`prometheus eval` measures the agent on task suites. Tasks run in fresh,
isolated sessions; grading happens afterwards and only there: the
*answer-free boundary* keeps grader material, reference answers and scores
away from the agent, harness evolution and the experience library.

```console
$ prometheus eval validate smoke
suite smoke 1 is valid (13 tasks)
$ prometheus eval run smoke --provider mock --run-id docs
run docs: 12 executed, 0 failed, 1 skipped, 0 resumed (<path>)
$ prometheus eval judge docs  # exits 5
ungraded: memory_write#1: not executed: skipped: needs memory
judged 12 of 13 rows (<path>)
$ prometheus eval report docs
score: 100.00
```

## Suites

A suite is a directory:

```text
suite.toml                 name, version, domain, default_timeout_secs (600),
                           default_toolsets, budget_usd, leak_globs
tasks/<task_id>/task.toml  prompt, domain, timeout_secs, toolsets,
                           capabilities, files (copied from files/)
tasks/<task_id>/files/     the task's workspace files
tasks/<task_id>/grader/    grader.toml and whatever it needs (never copied
                           into a workspace)
```

Offline suites may also give a task a `mock.toml` (the mock-provider script
its model plays when the run's provider is `mock`) and a `run.toml`: `set`
(configuration overrides, `key=value`), `config` (a TOML fragment),
`then` (follow-up prompts on the same session),
`interrupt_after_turn_start` and `requires` (a capability; tasks needing
one this run lacks are skipped and listed). The bundled `smoke` suite uses
all of them. Eval task sessions never read or write the profile's memory,
so the smoke suite's memory-writing task (`requires = "memory"`) is
skipped by `eval run`; `prometheus smoke-suite`, which gives every task a
scratch home, runs it.

`prometheus eval validate <suite>` checks everything and exits 65 naming
the first invalid file and problem (`tasks/t1/task.toml: missing key
'prompt'`). Suites are found by path, under `<profile>/evals/<name>/`, or
bundled (`smoke`, written to `<profile>/evals/.bundled/`); `prometheus eval
list` shows them.

## Runs

`prometheus eval run <suite> [--provider P] [--model M] [--harness REF]
[--tasks a,b] [--repeat N] [--parallel N] [--cache off|read|read_write]
[--budget-usd X] [--run-id ID] [--toolset-distribution D] [--seed N]` runs
every selected task and repeat in its own hidden session (`source = eval`,
tagged with the run id, a child of the run's hidden root session), at most
`--parallel` (4) at once. Each session gets a workspace built from the
task's files only; its sandbox reads nothing else, memory is off and
approvals fail closed. The model-call cache defaults to `read_write`.

`<profile>/evals/runs/<run_id>/` then holds `run.json` (suite and version,
provider, model, harness ref and hash, build, cache mode, toolsets,
started/finished) and, per task and repeat, `tasks/<task>/<repeat>/` with
`trajectory.jsonl`, `deliverables/` (`answer.md` and the workspace as
`files/`), `result.json` (`ok`, `delivered`, `cost_usd`, `wall_secs`,
`error`, never a score) and the completion marker `done.json`.

The run exits 0 when every task executed, 1 when one failed to execute and
3 when the budget stopped it. `--budget-usd` (or the suite's `budget_usd`)
is the run root's `budgets.root_usd`: a model call whose worst case would
exceed it is refused, and tasks not yet started are marked `skipped:
budget`. Eval sessions count as unattended, so an unpriced model also needs
`budgets.allow_unpriced` under `budgets.unattended_root_usd`.

Running again with the same `--run-id` resumes: tasks whose completion
marker still matches their deliverables are skipped, others run again after
`re-running <task>: <reason>` (`deliverables changed since completion`, `no
completion marker`, `it was skipped (budget)`). A different suite version,
model or harness hash is refused with exit 2.

### The answer-free boundary

Grader directories never reach a workspace and are outside the session's
sandbox. A task whose trajectory shows an attempt on a path inside a
`grader/` directory, or on a path matching the suite's `leak_globs`, is
marked `contaminated` in `result.json` and is excluded from scoring. What
harness evolution and selection may see of a result is `ok`, `delivered`,
`cost_usd`, `wall_secs` and `error` (`TaskResult::for_evolution`).

## Judging

`prometheus eval judge <run_id> [--parallel 8] [--judge-namespace NAME]` is
the only code path that reads grader material. `grader/grader.toml` names
a `type`:

| Type | Fields | Scores |
|---|---|---|
| `exact` | `expect`, `of` | normalised string equality (whitespace at the ends and of each line aside) |
| `regex` | `pattern`, `of` | the pattern matches |
| `script` | `command` | a sandboxed command in `grader/` (deliverables as its last argument and `PMT_DELIVERABLES`, no network) prints `{"n_passed", "n_criteria"}` |
| `llm` | `rubric` or `rubric_file`, `model` | a judge model on the `eval_judge` auxiliary route (the grader's `model` wins), recorded with purpose `eval.judge` |

`of` is `answer` (default), `exit` (the exit status a one-shot run would
have had) or `file:<path>` (a deliverable file). `judged.json` rows are
`{task_id, repeat, n_passed, n_criteria, all_pass, frac, graded, error}`.
A grader failure or zero criteria is an ungraded row (`frac = null`), never
a zero; tasks that did not execute or are contaminated are ungraded without
running the grader. The command exits 0 when every row is graded and 5
otherwise.

Judging again merges rows by `(task_id, repeat)` and keeps a graded row
when the new attempt is ungraded. `judged.json` records its judge (model
and grader version, a fingerprint of the suite's graders); another judge
is refused with exit 1 (`judge changed for this run (<old> → <new>); use
--judge-namespace to score separately`), and `--judge-namespace NAME`
writes `judged.<NAME>.json` instead.

## Reports

`prometheus eval report <run_id>... [--format text|markdown|json]` prints
each run's `score` (mean `frac` × 100), `all_pass` percentage, graded and
ungraded counts, contaminated tasks, cost, unpriced calls, tokens and wall
time; runs with `--repeat N > 1` add pass@1, pass@N and each task's
standard deviation. With two or more runs it adds the paired per-task
deltas against the first run and the score delta with a 95% bootstrap
confidence interval (10,000 resamples, seed 0), and `NOTE: difference
within noise` when the interval contains 0.

## Benchmark adapters

`prometheus eval run bench:<name>[:<split>]` runs an external benchmark's
tasks with the same layout. An adapter is a subprocess in
`<profile>/evals/bench/<name>/` (`adapter.toml`: `command = ["python3",
"adapter.py"]`) speaking JSON-RPC 2.0 over stdio, one message per line:
`bench.describe` (name, version, domains, `leak_globs`), `bench.tasks
{split}`, `bench.task {task_id}` (prompt without grading criteria, domain,
`files: [{path, content}]`, timeout), `bench.prepare {task_id, workspace}`,
`bench.deliverables {task_id, run_dir}`, `bench.preflight` and
`bench.score {task_id, run_dir}`. `bench.score` is called only by `eval
judge` (a null or failed score is ungraded); a score the adapter pushes
while tasks execute is refused with error `-32002`.

An adapter can also ship as a plugin (a subprocess adapter of the same
name wins):

- a WASM plugin exporting the `bench` world (`worlds = ["bench@1.0"]`,
  `wit/bench/world.wit`) provides `bench:<plugin id>`; `plugin.call(method,
  params)` answers each method with the same JSON. It has no filesystem:
  the `files` import (`read`, `write`, `entries`) reaches only the task
  workspace during `bench.prepare` and the task's run directory during
  `bench.deliverables` and `bench.score` (no `..`, absolute paths or
  symbolic links), and `requests.request` is its request to the host
  (`plugins/samples/bench` is a complete adapter);
- a Python-bridge plugin calls `ctx.register_bench_adapter(adapter)`;
  `adapter.name` is the name and its methods `describe`, `tasks`, `task`,
  `prepare`, `deliverables`, `preflight` and `score` take the protocol's
  params as keyword arguments, plus `host` when they declare it
  (`host.read(path)`, `host.write(path, data)`, `host.list()` on the
  same directory, and `host.request(method, params)`, which raises
  `HostError` with `rpc == -32002` for a score pushed during execution).

## Capturing sessions

`prometheus eval capture <session|recording.pmtrec> --suite DIR [--task-id
ID] (--expect TEXT | --expect-regex RE | --grader-script PATH)` turns a
real session into a regression task: its first user message becomes the
prompt and its recorded workspace the task files. A recording without the
workspace contents (exported without `--include-workspace`) is refused
with exit 65.

## Batch datasets

`prometheus eval batch <dataset.jsonl>` runs JSON lines (`prompt`; optional
`id`, `image`, `toolsets`, `files`) as ungraded tasks with the same
sessions, layout, cache, budget and resumption: `--parallel` (4),
`--max-turns` (10), `--max-samples N` (the first N lines),
`--reasoning-effort`, `--toolsets a,b` or `--toolset-distribution`,
`--prefill FILE` (a JSON list of messages inserted before each prompt) and
`--ephemeral-system-prompt TEXT|@FILE`, which is sent with every model call
and never stored: it is scrubbed from the sessions' recordings and is in no
trajectory, export or `result.json`. A line's `image` runs that task's
terminal in that container image, checked (or pulled) before any model
call; a missing one fails only that task (`image not available: <image>`).
An invalid line fails with exit 65 naming it before anything runs.

Every batch writes `statistics.json`: task counts (`completed`, `failed`,
`skipped`, `filtered`), tokens and cost in all and per model with
`unpriced_calls`, the call, success and failure counts of every tool
of the enabled toolsets (unused ones zero-filled), the fraction of
assistant turns with reasoning and the turns per task. Tasks with no
assistant reply, a final turn that hit `--max-turns`, or only failed tool
calls are `filtered`; `prometheus trajectory export --eval-run <id>`
leaves them out unless `--include-filtered`.

## Toolset distributions

`--toolset-distribution <name|file>` rolls each key of a distribution
(toolset names or `+`-joined compound keys, each with a probability) per
task, seeded by `--seed` (0) and the task id, so a seed reproduces every
task's toolsets; a compound key enables all of its members on one roll and
a task whose rolls all fail gets the most probable key. The sampled
toolsets are in `run.json` and each `result.json`. A TOML file `[toolsets]
web = 0.9` defines one; unknown toolsets are refused with exit 65.

```console
$ prometheus eval distributions
browser_tasks	browser+web=0.97 vision=0.3 terminal=0.1
```

## Compressing trajectories

`prometheus trajectory compress <input.jsonl> [-o FILE]
[--target-max-tokens 15250] [--summary-target-tokens 750]
[--protect-last-turns 4] [--sample-percent P] [--model M] [--parallel N]`
rewrites `ShareGPT` records over the target (tokens estimated at four
characters each): the head through the first system, human, gpt and tool
turns and the last turns stay verbatim, and the smallest middle span that
brings the record under the target becomes one `human` summary turn from
the `trajectory_compress` auxiliary route (purpose `trajectory.compress`).
Records under the target are copied without a model call; records still
over it go to `<output>.over_limit.jsonl`; `compress-stats.json` beside
the output reports counts, tokens before and after, and cost.

## The self-improvement demo

`prometheus demo --evolve [--k 2] [--mode parallel|sequential|both]
[--output DIR]` (or `prometheus evolve demo`) runs two ledger tasks of the
`accounting` domain through harness evolution offline: see
[The offline demo](demo.md#the-self-improvement-demo).

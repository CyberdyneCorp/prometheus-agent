# The harness and its evolution

The **harness** is the editable text around the model: a system prompt
overlay, guardrails, a memory policy, tool guidance, subagent definitions,
skills, scripts and hooks. Every profile has one, versioned under
`<profile>/harness/`, and harness evolution can propose better versions
for a concrete task. Evolution only ever *proposes*; the harness you run
changes when you activate a version, and an open session keeps the version
it started on.

## The tree

A harness version is a directory tree. Only these paths may be in it (the
editable surface, components c1 to c8):

| Path | Component |
|---|---|
| `SYSTEM.md` | c1 system prompt overlay |
| `GUARDRAILS.md` | c2 guardrails |
| `MEMORY_POLICY.md` | c3 memory policy |
| `TOOL_GUIDANCE.md` | c4 tool guidance |
| `agents/<name>/AGENT.md` | c5 subagent definitions |
| `skills/<name>/SKILL.md`, `skills/<name>/scripts/<file>` | c6 skills |
| `scripts/<file>` (`.py` or `.sh`) | c7 scripts |
| `hooks.toml` | c8 hooks |

The four always-on files (c1-c4) are injected, in that order, as the
harness layer of every new session's system prompt. It opens the
`profile` tier, right after the built-in `static` tier (identity, tool and
model guidance) and before skills, project context and memory, so adopting
a version keeps the cached `static` block (see
[System prompt layers](../reference/system-prompt.md)).
Harness skills and subagents are registered for the sessions that start
on that version. Your memory, `config.toml`, secrets and installed skills
are never part of a harness.

`hooks.toml` holds `[[hooks]]` entries with exactly `event` (a name of the
hook registry), `command` (an interpreter and a harness script, or the
script itself), `timeout_secs` (1-120, default 30) and `matcher`. A hook
runs in the profile's sandbox under the grants sessions already have;
nothing in a harness can ask for network access, secrets, capabilities or
another approval mode.

A fresh profile starts on the built-in version `h1` (empty always-on
files):

```console
$ prometheus harness log
* h1-<fingerprint> builtin job - <created> 4 files, 0 always-on chars, 0 skills, 0 agents, 0 scripts, 0 hooks
```

## Validating, editing and rolling back

`prometheus harness validate [DIR]` checks a tree (default: the active
version) and prints every error at once, exiting 65 when it is invalid:
paths and surface, size caps (24,000 always-on characters by default,
`harness.always_on_cap_chars`, 64 KiB per file, 1 MiB per tree, 200
files, 32 skills, 8 subagents), skill and subagent front matter, script
syntax (parsed, never run: `.py` by the built-in Python grammar, `.sh` by
`bash -n`, or `sh -n` without bash, so validating a tree with shell scripts
needs a shell on `PATH`), script references, `hooks.toml`, template
syntax in always-on text and secrets. A skill that no always-on file
names gets a trigger line appended to `GUARDRAILS.md`.

```console
$ prometheus harness validate
valid: 4 files, 0 always-on chars, 0 skills, 0 agents, 0 scripts, 0 hooks
```

These commands work whether or not evolution is enabled:

```sh
prometheus harness show h3 SYSTEM.md      # one file of a version
prometheus harness diff h3 h5             # unified diff and summary
prometheus harness apply ./my-harness     # validate and activate a new manual version
prometheus harness edit GUARDRAILS.md     # edit a copy of the active version in $EDITOR
prometheus harness rollback h3            # activate a new version with h3's tree
```

Versions are never deleted: a rollback writes a new version (`source =
"rollback"`) with the old tree. Every activation takes effect from the
next session; `--now` instead marks open sessions so that each rebuilds
its prompt at its next turn, which breaks that session's prompt cache
(logged as `prompt cache invalidated by harness change`).

## Evolution jobs

Evolution is opt-in per profile:

```console
$ prometheus evolve list # exits 78
error: harness evolution is disabled for profile 'default'; set evolve.enabled = true
```

With `evolve.enabled = true`, a job is queued from a session or a task
file:

```sh
prometheus evolve run --session ses_01J... --k 2 --mode parallel
prometheus evolve run --task tasks/weekly-report.toml --domain reports --budget-usd 2
```

A task file is TOML with `statement` (or `statement_file`) and optional
`domain`, `workspace` (relative to the file), `toolsets` and `model`. A
job's domain is `--domain`, else the scheduled job's or kanban task's
`domain` (or the task file's), else the source session's domain tag,
else one classification call that picks among the profile's existing
domains and `general`. The
budget k (1-8, default `evolve.k = 2`) fixes the search: each mode spends
4k rollouts and 3k improver sessions (`--mode both` runs both modes on
shared base rollouts). The job's task is frozen under
`<profile>/evolve/jobs/<job>/task/`: the statement, a snapshot of the
workspace (for a git repository its HEAD commit and an
`uncommitted.patch`), with files matching `evolve.leak_globs` left out
(a matching file already committed is listed under `excluded` in
`repo.toml` and deleted when the workspace is rebuilt),
secrets redacted and the size capped by `evolve.task.max_source_bytes`.

Jobs are also queued by `/evolve [--k N]` in a chat (it answers at once
and never adds work to the turn), by a scheduled job created with
`prometheus cron add <schedule> --evolve-task <file>` (the file relative to
the profile directory), by a kanban task of kind `evolve`, and, with
`evolve.auto.enabled = true`, after a session that ended in error or
interruption after a tool call or ran at least `evolve.auto.min_iterations`
iterations (at most `evolve.auto.max_jobs_per_day` a day; sessions of a
job itself never qualify). An automatic job is announced where the session
ended: `harness evolution job <id> queued after this session` on stderr or
in the line chat, in the messaging chat, or to the TUI, desktop or
dashboard still attached to it. A daemon session that ends after its last
client left (closed when idle) has nobody to tell: the job is in
`prometheus evolve list` and the daemon log.

## How a job runs

`prometheus evolve run` runs the job in the foreground after printing a
spend estimate, and exits 0 when it ends `ok` and 1 otherwise; with
`--detach` it only queues the job, and the daemon's evolution runner
starts queued jobs (and, with `evolve.resume_on_start = true`, queues
again the jobs a stopped process left running). A job:

1. **Rolls out** the active harness `k` times (`base1..basek`): each
   rollout is a hidden, headless `source = "harness"` session on a fresh
   copy of the task's workspace (a detached checkout of the recorded
   commit from a `--shared` clone, the patch applied, `excluded` paths
   deleted; or a copy of `task/source/`), on `evolve.rollout.backend`
   (`local-sandbox` by default; the unsandboxed host is refused), with
   every approval denied, no memory, egress limited to the configured
   model providers (the endpoint host, configured `base_url` or built-in,
   of every `[providers.<id>]` block and of the main route's provider;
   `evolve.rollout.network = "session"` keeps the profile's rules) and at most `evolve.rollout.max_iterations` model
   calls in `evolve.rollout.timeout_secs`. A tool that declares `effect =
   "external"` (sending a message, scheduling a job, writing memory,
   delegating...) is recorded instead of run and answers `recorded, not
   executed during harness evolution`; the terminal stays live inside the
   rollout's sandbox. A candidate's rollouts run with that candidate's
   whole harness, its `hooks.toml` included (replacing the active
   version's hooks for those sessions), so hook edits are tested too.
   Your workspace is never touched.
2. **Proposes**: improver sessions edit a private copy of the evidence
   (`context/task.md`, `source/`, the rollouts' trajectories and
   deliverables, the harness to edit, and for later steps a pool of
   earlier candidates with their diffs, rollouts and decision notes). An
   improver has only the file tools, confined to its copy: it can read a
   candidate's script but never run it. Its edit is accepted only when it
   validates and passes the leak tripwire; a rejected edit is retried
   once with the errors, then the step falls back to its base tree.
   `parallel` mode runs `inv<i>`/`dom<i>` stages on identical evidence and
   composes `agg1..aggk` from all of them; `sequential` mode runs `k`
   rounds, each composing checkpoint `c<t>` from the whole history.
3. **Selects** one final per mode blindly: the finals get anonymous ids
   (`A`, `B`...) and a selector session sees only the task and each
   candidate's result. With `evolve.selection = "user"` the job pauses in
   `awaiting-selection`; `prometheus evolve compare <job>` shows the
   anonymous candidates and `prometheus evolve choose <job> <ID>`
   continues it.
4. **Proposes** the chosen tree: `prometheus evolve proposals`,
   `prometheus evolve review <proposal>` (the diff against its base, the
   census, the selector's reason and the lesson), `prometheus evolve adopt
   <proposal> [--now] [--rebase]` and `prometheus evolve reject
   <proposal>`. Before the proposal is ready, a reflector session reads
   the whole search and distils at most one transferable lesson; adopting
   a proposal that carries one files it in the experience library of the
   job's domain ([The experience library](experience.md)).
   `evolve.adoption.policy` is `ask` (default), `auto` (adopt
   at once, except proposals adding or changing scripts, hooks or
   subagents, which always wait for you) or `never`. With mode `both`,
   at most one proposal is adopted: adopting one supersedes the other,
   and an automatic adoption that fails leaves its proposal pending. An adopted proposal
   becomes a new harness version (`source = "evolve"`) from the next
   session. A proposal whose base is no longer the active version is
   refused (exit 65) unless `--rebase` applies its diff cleanly.

Every improver session and rollout is cached by content under
`<profile>/evolve/cache/`, so `prometheus evolve resume <job>` (or the
daemon after a restart) re-runs only what is missing or changed; resuming
a finished job makes no model call. A job stops (`budget-exceeded`) once
its spend reaches `--budget-usd` or `evolve.budget.max_usd`, and
`prometheus evolve cancel <job>` stops it at the next model-call
boundary. `prometheus evolve status <job>` shows the stages, the
selection and the spend by kind; the job's `summary.json`, `ledger.json`
and `events.jsonl` are in its directory. `evolve.concurrency.sessions` and
`evolve.concurrency.rollouts` bound the work of every job of one process,
and one job per profile and domain runs at a time, in queue order.

Nothing a grader produced may reach evolution: files matching
`evolve.leak_globs` or holding an eval verdict (the `pmt-eval-verdict`
canary, or a `"pass_rate"`/`"passed_count"`/`"score"` number) are left
out of the task and fail any step whose evidence holds them. `prometheus
evolve review <proposal> --eval <suite>` runs any eval suite (a
directory, a name under `<profile>/evals/`, the bundled `smoke` or
`bench:<name>`) through the eval engine on the proposal's base version
and on the proposed harness, judges both runs and shows you the scores
and the delta with its confidence interval. The proposed tree is not a
version: it is materialised as a temporary harness for that run only, so
the review writes no version. The verdict goes to
`<profile>/eval-results/` and never into a proposal, a version or an
improver's evidence; the two runs stay under `<profile>/evals/runs/`
(`prometheus eval report <base> <proposed>` shows them again).

The same operations are served to the dashboard and the desktop as the
`evolve.*` and `harness.*` methods of the client protocol.

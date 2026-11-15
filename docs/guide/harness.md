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
syntax (parsed, never run), script references, `hooks.toml`, template
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
`domain`, `workspace` (relative to the file), `toolsets` and `model`. The
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
iterations (at most `evolve.auto.max_jobs_per_day` a day). This build
queues jobs; running them (rollouts, improver sessions, selection and
proposals) is the evolution engine's.

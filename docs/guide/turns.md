# Turns and the system prompt

Every surface (the CLI, the gateway, the TUI, cron jobs, delegated
subagents) runs a turn through the same loop. This page explains what a
turn does, how it ends, and what the model sees in its system prompt.

## How a turn runs

A turn starts when a message arrives for a session:

1. **Admission.** Only one process runs a turn on a session at a time. The
   turn takes the session's turn lease in `state.db` (it lapses after 5
   minutes unless renewed, and the running turn renews it every minute). A
   second process waits up to 30 minutes for the lease, then ends its turn
   with `lease_unavailable` without calling the model. If the lease is
   taken away mid-turn (for example by an operator deleting it), the turn
   stops with `interrupted_by_system`.
2. **The system prompt** is built once per session (see below) and
   replayed byte for byte on every later turn.
3. **Iterations.** Each iteration checks for interrupts and budgets,
   assembles the request from the frozen prompt and the stored transcript,
   calls the model, stores the reply and then either runs the requested
   tools (and loops) or returns the answer.
4. **The finalizer** emits `turn.end` with the exit reason.

## Exit reasons

A turn always ends with exactly one of these reasons:

| Reason | When |
|---|---|
| `completed` | The model gave a final answer. |
| `interrupted_by_user` | You stopped the turn. |
| `interrupted_by_system` | The system stopped it (lost turn lease, shutdown). |
| `iteration_limit` | `agent.max_iterations` model calls were made. |
| `token_budget_exhausted` | A `budgets.*_tokens` limit would be exceeded. |
| `cost_budget_exhausted` | A `budgets.*_usd` limit would be exceeded, or the model has no price under a USD budget. |
| `run_budget_exhausted` | The wall-clock budget (`agent.run_budget_secs`) ran out. |
| `guardrail_stop` | A loop guardrail stopped the turn. |
| `empty_response` | The model answered with neither text nor tool calls. |
| `context_exhausted` | The conversation no longer fits the model's context window. |
| `provider_failed` | The provider failed and nothing could recover. |
| `lease_unavailable` | Another process kept the session's turn lease. |
| `crashed` | Only for a turn recovered after a crash. |

## Iteration limit and budgets

`agent.max_iterations` (default 500, `0` = unlimited) caps the model calls
of one turn. A delegated subagent has its own budget; it never uses up its
parent's.

When a turn runs out of iterations, or a token or cost budget refuses the
next call, Prometheus does not just stop: it adds the notice

> You have reached this turn's budget. Give a final response summarizing
> what you found and did so far, without calling tools.

and sends one request without tools (retried once if the answer is
empty). Its text becomes the turn's answer and the exit reason stays the
budget one. That summary request does not count against the iteration
limit, but its tokens and cost are recorded like every other call.

Budgets are described in [Usage, costs and budgets](usage-and-budgets.md).
A model without a known price under a USD budget is never called: the turn
ends with `cost_budget_exhausted` and the error names the model.

## The system prompt

The prompt has three tiers, each sent as its own cached block: `static`
(identity and guidance), `profile` (platform hint and skills) and `session`
(personality, project context, memory and the rest, ending with the runtime
environment block). The [system prompt layers](../reference/system-prompt.md)
reference lists every layer in order.

Sessions of the same profile and model share the first two blocks byte for
byte, so the provider's prompt cache serves them.

**The prompt is frozen per session.** It is stored with the session at its
first turn; every later turn, including after a restart, sends exactly the
same bytes even if `SOUL.md`, the memory, the skills or the personality
changed on disk. It is rebuilt only for a new session, a model or provider
change, a compaction, or an explicit `--now`, and each rebuild records a
`prompt.rebuilt` event with its reason. Context that changes per request
(memory recall, plugin context) is added to the current user message, never
to the system prompt or to earlier messages.

### Identity

The identity is the profile's `SOUL.md` when it exists and is not empty,
otherwise "You are Prometheus, a personal AI agent.". `prompt.load_identity
= false` always uses the default. Disabling context files never disables
the identity.

### Project context files

Prometheus loads one kind of project context, the first that exists:

1. `PROMETHEUS.md` or `.prometheus.md`, nearest to the working directory up
   to the git root (front matter removed);
2. the `AGENTS.md` chain from the git root down to the working directory:
   in each directory the first non-empty of `AGENTS.override.md`,
   `AGENTS.md`, `agents.md` (identical files are loaded once);
3. `CLAUDE.md` in the working directory;
4. `.cursorrules` and `.cursor/rules/*.mdc` in the working directory.

Outside a git repository only the working directory is searched. Turn it
off with `prompt.load_context_files = false` (or `--no-context-files`).

Each file is checked for prompt-injection patterns (instructions to ignore
earlier instructions, to hide things from you, to print secrets, invisible
characters). A file that matches is replaced by
`[BLOCKED: <file> contained potential prompt injection (<findings>). Content not loaded.]`.
Files longer than `prompt.context_file_max_chars` (default: 6% of the
model's context window, between 20,000 and 500,000 characters) keep their
first 70% and last 20% with a marker in between. A file that cannot be read
within `prompt.context_file_read_timeout_secs` (default 5), for example on
a hung network mount, is skipped with a warning.

### Platform hints

Each surface has a built-in hint. Change it per platform:

```toml
[prompt.platform_hints.telegram]
replace = "Short replies."
append = "Use Markdown."
```

### Personalities

`agent.personality` selects an overlay added to the session tier, after the
identity: `helpful`, `concise`, `technical`, `creative`, `teacher`,
`kawaii`, `catgirl`, `pirate`, `shakespeare`, `surfer`, `noir`, `uwu`,
`philosopher` or `hype` (default `none`). Add your own or override a
built-in:

```toml
[agent.personalities]
reviewer = "Review like a strict senior engineer."
```

Select one in the config (it applies from the next session):

```console
$ prometheus config set agent.personality pirate
$ prometheus config get agent.personality
pirate
```

`/personality` lists them, `/personality <name>` selects one for the next
session, `/personality none` clears it, and `--now` applies it to the
running session (one prompt-cache break). Custom texts are checked like
context files.

### Session id

`prompt.include_session_id = true` (or `--pass-session-id`) adds
`Session ID: ses_<ULID>` to the session tier.

### Measuring the prompt

`prometheus prompt size` builds the prompt and tool schemas a new session
would send, without calling a model or reading credentials, and reports
bytes and estimated tokens per layer and per toolset:

```text
prometheus prompt size --platform telegram --toolsets core --json
```

`--platform` takes a surface with a built-in hint:

```console
$ prometheus prompt size --platform fax # exits 2
error: unknown platform 'fax' (valid: cli, tui, desktop, acp, api, cron, telegram, discord, slack, whatsapp, signal, matrix, email, sms)
```

## Context references

On the CLI, TUI, desktop and ACP surfaces a message can attach context with
`@` references; messaging platforms pass them through unchanged.

| Reference | Attaches |
|---|---|
| `@file:src/main.rs` | the file (`:10` one line, `:10-20` a range) |
| `@folder:src` | a tree listing with sizes (at most 200 entries) |
| `@diff` / `@staged` | unstaged / staged changes |
| `@git:3` | the last 3 commits with patches (1 to 10) |
| `@url:https://...` | the page, through the web extraction tool |

Content is appended under `--- Attached Context ---`. Paths must stay
inside the workspace; credential files (`~/.ssh`, `~/.aws`, `~/.gnupg`,
`~/.kube`, shell rc files, `~/.netrc`, `~/.pgpass`, `~/.npmrc`,
`~/.pypirc`, the profile's `secrets.toml` and `grants.toml`) and binary
files are refused. Every refusal is a warning in the message, never an
error. Above 25% of the model's context window a warning is added; above
50% nothing is attached and the message is sent as typed.

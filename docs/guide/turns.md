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
   delivers pending steer messages, assembles the request from the frozen
   prompt and the stored transcript, calls the model, stores the reply and
   then either runs the requested tools (and loops) or returns the answer.
   Each iteration ends with a `turn.step` event.
4. **The finalizer** emits `turn.end` with the exit reason and the
   session's prompt-cache hit rate.

A turn that ends without a final answer (any reason but `completed` and
`context_exhausted`) after your message or a tool result was stored gets a
closing assistant row, marked as written by Prometheus, with the error
(secrets redacted, at most 2,000 characters). Your next message therefore
always starts a new request instead of merging into the failed one.

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

## Interrupts and steering

Stopping a turn ends it at the next safe point with `interrupted_by_user`
(or `interrupted_by_system`). A hard stop (Ctrl-C, `/stop`) also cancels
running tools and delegated children and drops steer messages not yet
delivered; a soft stop lets the running tools finish and keeps them for
your next message. Stopping one session never affects another.

A steer message (`session.steer`) reaches the model without restarting
the turn. It is never added to anything already sent: after the next tool
results it becomes a new message wrapped in
`[OUT-OF-BAND USER MESSAGE kind=note]` ... `[/OUT-OF-BAND USER MESSAGE]`
(several steers are joined in arrival order). If the turn ends first, the
steer opens your next message. An empty steer is refused with
`empty steer`.

## Failures and retries

Each model call is tried up to `agent.api_max_retries` times (default 3)
with jittered backoff, then on the configured fallback models. When all of
them fail with a temporary error (rate limit, overload, server error,
timeout, network) and no answer text reached you yet, the turn waits and
calls again, up to `agent.auto_recovery_cycles` times (default 5, `0`
turns it off) after 15, 30, 60, 60 and 60 seconds; a provider's
`Retry-After` of up to 120 seconds is used instead. Authentication,
billing, entitlement, content-policy and malformed-request errors end the
turn with `provider_failed` at once.

Before every request, a copy of the transcript is repaired for the
provider: consecutive user messages are merged, a tool call without a
stored result (after a crash or a hard stop) gets a placeholder result, a
result without its call is dropped, and tool arguments that are not JSON
are replaced by a stub. The stored transcript never changes.

## Guards on the loop

**Repeated tool calls.** Prometheus counts, per turn, the same call failing
with the same error, the same tool failing, and a read-only call returning
the same result again. From the second identical failure (third failure of
one tool, second identical read) the tool result carries a warning for the
model. Five identical failures (eight of one tool, five identical reads)
end the turn with `guardrail_stop` on gateway, cron, API and kanban
sessions; interactive sessions only warn unless
`tool_loop_guardrails.hard_stop_enabled = true`. Every turn allows at most
50 [`web_search`](../reference/tools.md#web_search) calls and 50 subagent
spawns (`tool_loop_guardrails.loop_caps.max_web_searches` and
`max_subagents`, `0` = unlimited); further calls are refused unrun.

**Empty answers.** An answer with neither text nor tool calls is retried up
to three times in a row, only once when a single attempt's input is
estimated at `agent.empty_response_guard.cost_threshold_usd` (default
$0.25) or more, and not at all when the model produced no output tokens
(it would refuse again). Then the turn ends with `empty_response`.

**Narration.** Some models announce an action ("Let me check the tests.")
and stop without calling a tool. Where `agent.intent_continuation` applies
(`auto`: models on the Responses API; `true`; `false`; or a list of model
name fragments) the turn adds up to two short "continue now and call the
tools" messages per turn.

**Wall-clock budget.** With `agent.run_budget_secs` (or `prometheus chat
--run-budget <secs>`), once 80% of the budget has elapsed the next tool
result tells the model, once, to produce the final deliverable now;
implicit stream idle timeouts are lowered to `max(60 s, half the remaining
time)` (never raised, never one you configured); and the turn ends with
`run_budget_exhausted` when the budget runs out.

## Prompt-cache health

Every model call records its uncached input, cache-read, cache-write and
output tokens. `/usage` and `turn.end` report the session's cache hit rate
(cache-read tokens over all input tokens). Each request is compared with
the previous one of the session: if anything it already sent changed
without a compaction or a recorded `prompt.rebuilt`, Prometheus emits a
`cache.unexpected_break` warning naming the first changed message (logged
as an error in debug builds), because such a change multiplies the cost of
every later request.

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

The prompt has three tiers, each sent as its own block: `static`
(identity and guidance), `profile` (platform hint and skills) and `session`
(personality, project context, memory and the rest, ending with the runtime
environment block). The [system prompt layers](../reference/system-prompt.md)
reference lists every layer in order.

Sessions of the same profile and model share the first two blocks byte for
byte, so the provider's prompt cache serves them (see
[prompt-cache breakpoints](compression.md#prompt-cache-breakpoints)).

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

# Context compression

A long session stays inside the model's context window through
*compaction*: old tool output is pruned, the middle of the conversation is
replaced by a structured handoff summary, and the session goes on under the
same id. Compaction is the only thing that ever rewrites context the model
has already seen, so it is also the only expected prompt-cache break.

## When it runs

The trigger is computed from the **main** model's window:
`floor((context_window - output cap) * threshold)`, never below 64,000
tokens and never above 85% of the input budget. `compression.threshold`
(default `0.50`) is raised to at least `0.75` for models under 512,000
tokens; a non-zero `compression.threshold_tokens` and a smaller window of
the compression model lower it further. Everything is re-derived on every
model switch.

`compression.model_thresholds` sets the base threshold per model: keys are
model-name substrings or `"<provider>:<substring>"`, the longest matching
key wins and a provider-qualified key beats a bare one with the same
substring; the small-window floor still applies on top:

```toml
[compression.model_thresholds]
"glm-5" = 0.40
"glm-5-1m" = 0.25
```

With `compression.enabled = true` (the default) the turn checks the context
at turn start, before each model request and after each tool round. It
uses the size the provider reported for the latest request plus estimates
of what was added since; with only a rough local estimate (a resumed
session's first request) the request is sent uncompressed and the reported
usage decides, unless the provider is known not to report usage. Each turn
allows `compression.max_attempts` attempts (default 3); attempts never use
up iterations.

Automatic compaction pauses when:

- a summary failed: a cooldown of 60 s, then 300 s, then 900 s;
- an attempt found nothing to summarize: a 60 s backoff;
- two compactions in a row reclaimed less than 10% of the tokens (the
  `ineffective` breaker): until a manual `/compress`.

These guards are kept in `state.db`, so a restart honours them.

If the provider still rejects a request as too long, Prometheus compacts
once (even during a cooldown) and retries. When the attempts are spent the
turn ends with `context_exhausted`: "Context is full. Run /compress here 2
to keep only the last exchanges, or /new to start a new session."

## What is kept

- **Head:** on a session's first compaction the first
  `compression.protect_first_n` messages (default 3); later compactions
  protect only the system prompt.
- **Tail:** the most recent messages under a token budget (`lean`: 2.5% of
  the window, 10,000 to 25,000 tokens; `ratio`: trigger x
  `target_ratio`; never above 20% of the window), always with at least
  `compression.min_tail_user_messages` user messages. A tool call is never
  separated from its results.
- **Middle:** everything else is summarized. Before that, without any
  model call, repeated tool output becomes
  `[duplicate tool output, same as a later call]` and tool results over 200
  characters become one line such as
  `[pruned tool result] terminal({"command":"make"}) -> 40000 chars, ref 1f2e3d4c5b6a`;
  the original stays in the session's blob store under that ref, and the
  agent reads it back with
  [`session_search`](../reference/tools.md#session_search) and
  `ref = "1f2e3d4c5b6a"` (within the normal tool-result size budget).

The summary comes from the auxiliary `compression` route (see
`[auxiliary.compression]`), within 20% of the summarized tokens (at least
2,000, at most 5% of the window and `compression.summary_max_tokens`).
On later compactions the previous summary is updated instead of rewritten.
It always has this shape:

```text
[CONTEXT COMPACTION - REFERENCE ONLY]

## Goal
## Progress
## Decisions
## Files
## Open Todos      (the open items of the todo list, verbatim)
## Pinned          (pinned messages, verbatim; only when there are any)
## Preserved References   (what the summary left out; only when needed)
## Next Steps
--- END OF CONTEXT SUMMARY - respond to the message below, not the summary above ---
```

Before it commits, the summary is checked against what it must not lose,
taken from the summarized messages: the files written or patched, the
open todo items, the latest user request and tool errors that were never
followed by a success of the same tool. When it mentions less than
`compression.quality_min_coverage` of them (default `0.8`) the summarizer is
asked once more with the missing items listed; anything still missing is
appended verbatim under `## Preserved References`. The share is recorded as
`coverage` in `compression.committed`.

If the summary fails, times out (`compression.summary_timeout_secs`) or is
cut off, a deterministic summary is committed instead (previous summary,
prune lines, files written, the latest request, unresolved tool errors),
unless `compression.abort_on_summary_failure = true` or the failure was an
authentication or quota error; then nothing changes.

Whatever engine produced it, the compacted transcript is validated before
it is committed: user and assistant messages still alternate where new
rows were written, every tool result follows its call, no call is left
without a result (except a call still running), it is smaller than before
and the current user message is unchanged. A result that fails is
discarded, the transcript stays as it was and `compression.failed` carries
reason `invalid_engine_output` (it arms the cooldown like a failed
summary).

The replaced messages are not deleted: they stay in the session marked as
archived with the compaction's epoch, so `prometheus sessions show`,
exports and [`session_search`](../reference/tools.md#session_search) still
find them. Each commit records
`compression.committed` (trigger, epoch, tokens before and after, summary
tokens, the quality coverage, duration); failures record `compression.failed`. Only one
compaction runs per session at a time; a second attempt reports
`lock_skipped`.

After a commit the system prompt is rebuilt from the current memory,
skills and plugin state (`prompt.rebuilt`, reason `compaction`), so the
next request starts a fresh cache prefix.

## `/compress`

```text
/compress [--preview] [here [N]] [<focus text>]
```

- `/compress` compacts now; it clears an armed cooldown, backoff or
  breaker first.
- `--preview` shows the estimated tokens before and after and how many
  messages would be summarized, without changing anything.
- `here [N]` keeps exactly the last N user exchanges (default 2) and
  summarizes everything before them.
- Focus text is passed to the summarizer as a priority ("/compress the
  database migration").

The result is one of `preview`, `nothing_to_do`, `lock_skipped` or
`compressed`.

## Pinned messages

```text
/pin [<message-id>|last]
/unpin <message-id>
```

`/pin` (default `last`: the newest user or assistant message) keeps a
message's text in the `## Pinned` section of every later summary, however
many compactions run; an id prefix is enough when it is unique. Pinned text
together stays within `compression.pinned_max_tokens` (default 8,000): a pin
over it is refused with `pinned content would exceed <N> tokens; unpin
something first`.

## Opt-in upkeep

These run only when configured; each commit is a compaction like any
other (validated, in place, the prompt rebuilt).

- **Micro-compaction** (`compression.micro_compact = true`): every
  `compression.micro_compact_every_n_turns` completed turns (default 1;
  interrupted or failed turns do not count) one assistant exchange of the
  compressible middle is folded into a single rolling summary row. User
  messages, the protected head and the tail are never folded, only the
  newest rolling summary stays in the context (an older one becomes a
  one-line marker), and an exchange that fails to fold three times in a row
  is skipped. A failure is logged and never fails the turn.
- **Prepared-ahead summaries** (`compression.prepare_ahead = true`): once
  the context passes 90% of the trigger the summary is written in the
  background. When the trigger is crossed it is used without another
  summarizer call if the conversation only grew since; otherwise it is
  discarded and the summary is written as usual.
- **Proactive prune** (`compression.proactive_prune_tokens`, default `0` =
  off): below the trigger, once the context passes this size, only the
  deterministic prune runs, on tool results over 8,000 characters outside
  the head and tail. It commits only when it reclaims at least 4,096 tokens
  and runs again only after the context has grown back by what it
  reclaimed, so cache breaks stay rare.

## Gateway session hygiene

Before the gateway hands an incoming message to the agent, it compacts the
session (trigger `gateway_hygiene`) when the last known context reaches 85%
of the main model's window or the session has 5,000 messages. The size is
the provider's last reported count, else the usage stored for the session's
last model call (since its last compaction), else an estimate. Sessions
with fewer than 4 messages, and profiles with `compression.enabled =
false`, are left alone.

## Idle compaction

With `compression.idle_compact_after_secs` above 0 (default 0, off), the
first model call of a turn that starts at least that many seconds after
the session's last message is preceded by a compaction with trigger
`idle`: a conversation picked up after a long pause has lost its prompt
cache anyway, so this is the cheapest moment to shrink it. A context
already at or below `threshold * target_ratio` of the input budget is left
alone. The usual lock, cooldown, backoff and per-turn attempt budget
apply.

## Progress notices and the context gauge

Every compaction emits `compression.started` (attempt 2 when the summary
is rewritten after the quality check), then `compression.committed` or
`compression.failed`. The CLI, the TUI and attached clients show automatic
compactions as notice lines; a manual `/compress` answers itself. In a
messaging chat, routine statuses (start, retry, completion, idle) are
posted only with `compression.progress_notices = true`; failures always
are.

After each turn the context is measured against the compaction trigger.
From 60% the CLI and the TUI print a gauge line such as
`context ████████████░░░░░░░░ 62% to compaction` (flagged `compacts soon`
from 85%); a messaging chat gets one plain-text notice each time the
session crosses into the 60% or the 85% level. None of this is ever added
to the model's context.

## Server-side compaction (Responses)

`compression.responses_native = true` (default false) asks a route whose
provider compacts server-side on the Responses wire (`openai-api`, or a
custom provider with `supports_server_compaction = true`) to compact once
the input reaches `compression.responses_compact_threshold_tokens`
(default 0: the local trigger minus 8192 tokens, or 200000 without a local
trigger). The opaque compaction item a response returns is stored as a
blob linked to the session and sent first in later requests in place of
the history it covers; the stored transcript keeps every message, and
local compaction still runs at its own trigger (a local compaction that
folds the covered history ends the replay). A route without support
ignores the setting with one INFO log per session.

## `/context`

`/context` (alias `/ctx`) shows the window without a model call and without
touching the prompt cache: a 5x20 grid of cells (each 1% of the window; a
text gauge on messaging platforms), an estimated breakdown (system prompt,
tool definitions, project rules, skills index, MCP tools, subagents,
memory, conversation, free), the compaction trigger and headroom, the
compaction count and time of the last one, the tokens processed so far,
and every candidate context file (`PROMETHEUS.md`, the `AGENTS.md` chain,
`CLAUDE.md`, Cursor rules, `SOUL.md`) with its estimated tokens and
whether it was `loaded`, `truncated`, `shadowed` (a file that wins over it
loaded instead), `blocked` by the threat scan, `unreadable` or
`suppressed`. `/context all` adds the schema cost of each toolset and the
index and full-load cost of each skill.

## Prompt-cache breakpoints

For providers with explicit cache breakpoints (Anthropic Messages) each
request carries at most four, all with the same lifetime: two at the end
of the last system-prompt tiers and the rest on the newest messages. They
are planned for each request and never stored.

`prompt_cache.ttl` is `5m` (default), `1h`, `off` or `auto`: one hour for
interactive sessions and five minutes for subagent, cron, webhook, kanban,
API and batch sessions, decided once per session. A provider that cannot
keep the chosen lifetime gets the longest it supports.

## Context engine

`context.engine` selects the engine: `builtin` (the default) or an engine
registered by an installed, enabled plugin. An unknown name fails
`prometheus config validate` with `unknown context engine '<name>'` (exit
78); a plugin's engine is used only when the key names it. The host
validates a plugin engine's output like the built-in one's.

```console
$ prometheus config set context.engine lcm
$ prometheus config validate # exits 78
error: unknown context engine 'lcm'
hint: registered context engine names: builtin
$ prometheus config set context.engine builtin
$ prometheus config get compression.threshold
0.5
```

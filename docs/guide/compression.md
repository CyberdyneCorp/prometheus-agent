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
  the original stays in the session's blob store under that ref.

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
## Next Steps
--- END OF CONTEXT SUMMARY - respond to the message below, not the summary above ---
```

If the summary fails, times out (`compression.summary_timeout_secs`) or is
cut off, a deterministic summary is committed instead (previous summary,
prune lines, files written, the latest request, unresolved tool errors),
unless `compression.abort_on_summary_failure = true` or the failure was an
authentication or quota error; then nothing changes.

The replaced messages are not deleted: they stay in the session marked as
archived with the compaction's epoch, so `prometheus sessions show`,
exports and [`session_search`](../reference/tools.md#session_search) still
find them. Each commit records
`compression.committed` (trigger, epoch, tokens before and after, summary
tokens, duration); failures record `compression.failed`. Only one
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
78); a plugin's engine is used only when the key names it.

```console
$ prometheus config set context.engine lcm
$ prometheus config validate # exits 78
error: unknown context engine 'lcm'
hint: registered context engine names: builtin
$ prometheus config set context.engine builtin
$ prometheus config get compression.threshold
0.5
```

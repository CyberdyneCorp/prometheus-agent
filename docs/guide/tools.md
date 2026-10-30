# Tools and toolsets

The model works through tools. Every tool belongs to a toolset, and a
session sees only the tools of the toolsets it selects that it is also
allowed and able to use. This page describes how that set is chosen, how
calls run and what happens to large results.

## Choosing toolsets

`tools.enabled_toolsets` (default `["core"]`) names the toolsets a session
starts with; `tools.disabled_toolsets` removes toolsets last, so a disabled
tool stays out even when a composite such as `debugging` lists it again.

```toml
[tools]
enabled_toolsets = ["core", "x_search"]
disabled_toolsets = ["browser"]
```

- A toolset can include other toolsets: `debugging` is `terminal` and
  `process` plus `web` and `file`; `safe` is `web`, `vision` and
  `image_gen` and never anything that executes.
- `coding` is a posture: every `core` tool except `image_generate`,
  `text_to_speech`, `cronjob` and `computer_use`. Naming it also selects
  the coding prompt posture.
- `x_search`, `video`, `video_gen` and `kanban` are default-off: they are
  never part of a default and appear only when named explicitly.
- `all` means every toolset except default-off and surface-gated ones.
- An unknown name is an error, never ignored: `prometheus config validate`
  exits 78 with `unknown toolset 'brwser' (did you mean
  'browser'?)`. Toolsets that include each other in a cycle are rejected
  with the cycle, for example `toolset include cycle: a -> b -> a`.

```console
$ prometheus config set tools.enabled_toolsets '["core", "brwser"]'
$ prometheus config validate   # exits 78
error: tools.enabled_toolsets: unknown toolset 'brwser' (did you mean 'browser'?)
$ prometheus config set tools.enabled_toolsets '["core", "x_search"]'
$ prometheus config validate
<...>config.toml is valid
```

## What a session exposes

A selected tool is exposed when the session's capabilities cover every
capability it declares (see [Capabilities, grants and
redaction](security.md)) and its availability check passes (a browser
runtime installed, a provider configured...). Every other tool has exactly
one reason: `not_in_toolset`, `disabled`, `missing_capability:<capability>`
or `unavailable:<reason>`. One missing runtime only hides the tools that
need it; the rest of `core` stays.

Availability checks are shared by every session of a profile for 30
seconds. A check that fails within a minute of succeeding is treated as a
flake: the tools stay available and the next session checks again.

The tool list is fixed when a session starts and sent unchanged on every
model call, which keeps the provider's prompt cache valid. Toolset, plugin,
credential and config changes apply from the next session (or a compaction
or explicit `--now` reload).

## How a call runs

Each call is checked before it runs: an unknown or hidden name returns
`unknown tool: <name>`, arguments that are not a JSON object or do not
match the tool's schema return every violation (`/path: expected string`)
without running the tool. Hooks, the capability check and the approval
gate come next; then the tool runs under its timeout
(`tools.default_timeout_secs`, 300 by default) and stops within two
seconds when you interrupt the turn.

Every call ends with a status: `ok`, `error`, `blocked`, `denied`,
`degraded`, `timeout` or `cancelled`. A tool that crashes becomes an
`error` result starting `[TOOL_ERROR]`; the turn goes on. Error text shown
to the model is stripped of role tags, CDATA markers and code fences and
cut to 2,048 characters.

Calls from one model response run at the same time (up to
`agent.tool_concurrency`, default 8) only when none of them writes files,
needs an exclusive resource or talks to you; otherwise they run one after
another. Results always come back in the order the model asked.

## Large results

A result longer than its budget is saved to
`<profile>/cache/spillover/<session>/<call>.txt` (readable only by you,
secrets redacted) and the model gets a `<persisted-output>` block with the
size, the file path and a 1,500-character preview instead.

| Budget | Default |
|---|---|
| `tools.result_budget.overrides.<tool>` | per tool |
| `tools.result_budget.mcp_chars` (MCP tools) | 50,000 |
| `tools.result_budget.default_chars` | 100,000 |
| all results of one response | 200,000 |

`read_file` is never spilled; it pages through files itself. With a model
whose context window is known, one result may use at most 15% of the window
and one response 30% (4 characters per token, at least 8,000 and 16,000
characters). If the file cannot be written the result is cut inline and
ends with `[Truncated: tool response was N chars; full output could not be
saved]`.

Spilled files are deleted after `sessions.retention_secs`, or after 24
hours when that is 0, and with their session (`prometheus sessions delete
<id>`).

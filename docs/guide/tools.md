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
- `coding` is a posture: every `core` tool except
  [`image_generate`](../reference/tools.md#image_generate),
  [`text_to_speech`](../reference/tools.md#text_to_speech),
  [`cronjob`](../reference/tools.md#cronjob) and
  [`computer_use`](../reference/tools.md#computer_use). Naming it also selects
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

The [tools reference](../reference/tools.md) lists every shipped toolset
and tool name. To see them in your profile:

```console
$ prometheus toolsets list
$ prometheus toolsets show core
$ prometheus tools list
$ prometheus tools list --session refactor   # exits 66
error: no session matches 'refactor'
$ prometheus toolsets show brwser   # exits 2
error: unknown toolset 'brwser' (did you mean 'browser'?)
```

`toolsets list` marks the active toolsets with `*` (`--surface cron` marks
those of `toolsets.surfaces.cron` instead) and shows each toolset's tools,
includes, resolved tool count and `default-off`/`posture` flags.
`toolsets show <name>` prints the tools a toolset resolves to, in order,
each `exposed` or with the reason it would not be. `tools list` prints every
registered tool with its toolset and the same verdict, and `tools explain
<name>` its schema, owner, capabilities, `effect`, `idempotent`, resource
claims, timeout, result budget, the active approval mode and the approval
rules that apply to its calls. `tools list --session <id>` prints the
surface that session froze when it built it (its id, an id prefix or its
title), so later config, grant or runtime changes do not hide what the
session was given; a session that never built a surface exits 66
(`tool.surface_unrecorded`). In a session, `/toolsets` prints the same
listing with that session's toolsets marked. An unknown toolset or tool
name exits 2 and suggests the closest names.

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

## Tools the agent runs itself

[`todo`](../reference/tools.md#todo),
[`clarify`](../reference/tools.md#clarify) and
[`session_search`](../reference/tools.md#session_search) need session state
only the turn loop has, so the turn loop runs them; dispatching one through
the generic path (for example from an RPC client) returns
`<name> must be handled by the agent loop`.

- [`todo`](../reference/tools.md#todo) keeps a prioritized list of
  `{id, content, status, parent?}` items (`pending`, `in_progress`,
  `completed`, `cancelled`; at most 256 items of 4,000 characters). Without
  `todos` it returns the list; `merge = false` replaces it; `merge = true`
  changes only the given fields of existing ids and appends new ones. Every
  write bumps `revision`, every call returns the whole list, the list is
  stored with the session, and after compaction only open items (and the
  parents of open subtasks) are shown again.
- [`clarify`](../reference/tools.md#clarify) asks you 1 to 5 questions in
  one prompt (each with at most 4 choices plus a free-text answer) and waits
  `agent.clarify_timeout_secs` (default 3600, 0 = no limit); when nobody
  answers the result says `[user did not respond within <N>m]`. Sessions no
  one attends (cron, API, `-z`) do not offer it.
- [`session_search`](../reference/tools.md#session_search) searches this
  profile's past sessions (never another profile's), leaving out what the
  current session already has in context, and can return the messages
  around a hit (`session_id`, `around_message_id`, `window`). Results are
  capped at 16 KiB.

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

[`read_file`](../reference/tools.md#read_file) is never spilled; it pages through files itself. With a model
whose context window is known, one result may use at most 15% of the window
and one response 30% (4 characters per token, at least 8,000 and 16,000
characters). If the file cannot be written the result is cut inline and
ends with `[Truncated: tool response was N chars; full output could not be
saved]`.

Spilled files are deleted after `sessions.retention_secs`, or after 24
hours when that is 0, and with their session (`prometheus sessions delete
<id>`).

## Approvals

Commands the model runs (the [`terminal`](../reference/tools.md#terminal)
and [`execute_code`](../reference/tools.md#execute_code) tools) pass an
approval gate first, always in this order:

1. **Hardline commands are blocked**, in every mode, backend and context:
   recursive deletes of `/`, your home or a system directory, `mkfs`,
   writes to raw disks, fork bombs, `kill -1`, shutdown and reboot, and
   deleting Prometheus itself or its home. The model gets
   `BLOCKED (hardline): <reason>`.
2. **Your deny rules are blocked** the same way: `approvals.deny` holds
   command globs (`*` any text, `?` one character) matched against the
   whole command and each command in it (`BLOCKED (deny rule '<rule>')`).
3. **`approvals.mode = "off"`** (or `--yolo`, or `/yolo` for one session)
   runs everything else without asking; the status line shows `YOLO`.
4. **Allow rules** in `grants.toml` (`[approvals] allow = [...]`) run
   without asking; a command line is allowed when it matches a rule as a
   whole or every command in it does.
5. **Dangerous commands ask**: recursive deletes, `git reset --hard`,
   force pushes, `chmod -R`, `sudo`, a download piped into a shell,
   destructive SQL and similar, plus two content checks: a `curl`/`wget`
   request body or upload that sends a secret-named variable (`$..._KEY`,
   `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL`) or a credential file
   (`.env`, `~/.ssh/*`, `*.pem`...), and invisible Unicode characters
   that can disguise a command. Sending a token in a header (`-H
   "Authorization: Bearer $TOKEN"`) is normal and does not ask.

You answer a prompt with `once`, `session` (until the session ends),
`always` (added to `[approvals].allow` in `grants.toml`; an entry you delete
by hand is never written back) or `deny`. No answer within
`approvals.timeout_secs` (300 s), a closed interface or a session without
any way to ask all mean deny, and the model is told not to retry.

Cron jobs, `chat -z` runs and webhook/API sessions never wait: they answer
from `approvals.cron_mode`, `approvals.single_query_mode` and
`approvals.unattended_mode` (`deny` by default, or `approve`); a denial
says which key to set. These are strings: `cron_mode = false` is a
configuration error (exit 78). `smart` mode asks like `manual` until the
guardian model arrives.

Writes to protected instruction files (`AGENTS.md`, `CLAUDE.md`, ...) ask
every time, in every mode including `off`, and a plugin rule that escalates
a call asks too (remembered as `plugin_rule:<rule>`).

Try a command against the gate without running it:

```console
$ prometheus approvals test --mode off -- rm -rf /
verdict: hardline
rule: recursive delete of root filesystem
outcome: blocked: BLOCKED (hardline): recursive delete of root filesystem
mode: off, context: interactive, backend: local
nothing was executed
$ prometheus approvals test --context cron -- git reset --hard
```

The verdict is one of `hardline`, `deny rule`, `bypass`, `allowlisted`,
`would prompt` or `allowed`; the command always exits 0, or 2 when the
command line cannot be read (an unterminated quote).

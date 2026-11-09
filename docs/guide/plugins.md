# Plugins

Plugins add capability at the edges: tools for the model, hook callbacks
around tool and model calls, system-prompt sections, slash commands,
skills, provider objects (web search, image generation...), and (as their
hosts land) platform adapters, memory providers, context engines, terminal
backends, UI panels and delegation routers. A native plugin is a WebAssembly component that
runs inside Prometheus with only the capabilities you approved; it never
gets ambient access to files, the network, the clock or environment
variables.

## Where plugins come from

A plugin is a directory holding `plugin.toml` (and, for a `wasm` plugin,
its component `plugin.wasm`). Prometheus looks in:

- the plugins built into the binary;
- `<profile>/plugins/<id>/`;
- `<project>/.prometheus/plugins/<id>/`, only when the project root is
  listed in `plugins.trusted_project_dirs` and the session runs inside it.
  Any other project's plugin directory is never read.

A project plugin hides a user plugin with the same id (`plugins list` shows
the hidden copy); no plugin may take a built-in plugin's id.

## Installing

```sh
prometheus plugins install weather                    # a catalog id, at its pinned ref
prometheus plugins install ./weather-1.0.0.pmtpkg     # a package file
prometheus plugins install https://github.com/acme/weather --ref <40-hex commit>
```

```console
$ prometheus plugins install https://github.com/x/y --ref main # exits 2
error: --ref must be a full commit SHA
```

Every install and update scans each plugin file in full with the skills
guard's threat patterns (reverse shells, credential exfiltration,
destructive commands, persistence, obfuscated execution, prompt
injection), and a line longer than 8 MiB fails the scan. A file that is
not wholly UTF-8 is scanned too, its invalid bytes read as U+FFFD, since a
shell still runs the lines around a stray byte; none of its lines counts
as a comment. A finding inside a WebAssembly module (a file starting with
`\0asm`) is data the sandboxed plugin holds, so it is lowered one step
like a fixture's, but still reported. A finding
in documentation, a changelog, a test fixture or a line that is entirely a
comment in the file's own language (judged by its extension: `#` in shell
and Python, `//` or a closed `/* */` with nothing after it in JavaScript,
Rust and Go, `#` or a closed `<# #>` in PowerShell; files of unknown
language, Ruby, Perl, Tcl, PHP and JSX have no comments, and a line is
never a comment when it holds a character an interpreter reads as a line
end, such as a lone CR or U+2028, or text that could close a string or
block comment opened on an earlier line or run an interpolation: quotes,
braces, `$`, `\`, `*/`, PowerShell's `#>`)
counts one step lower; bundled skills keep full weight. `dangerous` is refused (exit
77, naming each finding, file and line) even with `--force`; `caution`
installs after a yes on the terminal or with `--force` (recorded on the
install record); a scan that fails refuses the install. A dangerous update
leaves the installed version running and is held on its install record:
`plugins check-updates` and `plugins update` report it as disabled until
reviewed (exit 77) without fetching it again, until the source publishes a
different version. `plugins.scan_on_install = false`
skips the scan, and the install record says so.

A package is checked before anything lands in the profile. Catalog installs
always need a signature that verifies against the catalog's publisher key;
a local package follows `plugins.signature_policy` (`require`, the default:
only with `--allow-unsigned`; `warn`; `off`). An invalid signature is
refused whatever the flags. An unsigned install shows the plugin's id,
digest and capabilities first. The plugin lands in `<profile>/plugins/<id>/`
with an install record (`.install.toml`: source, ref, digests, signer,
tier, date, flags), disabled unless you pass `--enable` or answer yes to
`Enable '<id>' now?`.

The install record is the plugin's provenance. A directory copied into
`plugins/` by hand shows as `unadopted` and never loads until `prometheus
plugins adopt <dir>` validates it, records it and asks you to approve its
capabilities. A plugin whose files changed after install shows `plugin
'<id>' was modified after install` and does not load.

The catalog is fetched from `plugins.catalog_url` (its `.minisig` must
verify against the keys built into Prometheus; at most 2 MiB), cached for
6 hours under the profile's `cache/plugins/`, and replaced by the catalog
built into the binary whenever it cannot be used. Its `removed` list is a
kill list: a plugin on it (by id or repository) does not install, enable or
load, and `plugins list` shows why, unless it was installed with
`--allow-removed`.

```console
$ prometheus config set plugins.catalog_url ""
$ prometheus plugins search weather
no catalog plugins match
$ prometheus plugins check-updates
every plugin is up to date
$ prometheus plugins doctor
ok no plugin problems
```

```sh
prometheus plugins browse                   # every catalog entry
prometheus plugins show weather             # installed state, worlds, provenance, catalog entry
prometheus plugins capabilities [weather]   # declared versus granted
prometheus plugins update [weather]         # catalog ref or package moved
prometheus plugins diff-grants weather      # what a waiting update asks for
prometheus plugins remove weather           # grants revoked, plugin-data kept
```

An update that asks for nothing new keeps its approval. One that asks for
more keeps the current version running and exits 4 with `weather 1.1.0
requests new capabilities: secrets GITHUB_TOKEN` until you approve it
(`prometheus plugins update weather --grant all`, or yes on a terminal).
[Writing a plugin](plugin-authoring.md) covers `validate`, `pack` and
`sign`.

## Enabling a plugin

Nothing found runs until you enable it. In a profile with no plugins:

```console
$ prometheus plugins list
no plugins found
$ prometheus plugins enable weather # exits 66
error: no plugin 'weather' was found
$ prometheus config set plugins.wasm.call_timeout_ms 10000
$ prometheus config set plugins.hook_timeout_ms 0 # exits 2
error: plugins.hook_timeout_ms: must be between 1 and 600000
```

With plugins installed:

```sh
prometheus plugins list                 # id, version, kind, source, state, problem
prometheus plugins enable weather       # shows what it asks for, asks [y/N]
prometheus plugins enable weather --grant all          # non-interactive
prometheus plugins enable weather --grant "network api.weather.example"
prometheus plugins disable weather
```

Enabling shows the plugin's capabilities in plain language and asks you to
approve them. Without a terminal, `--grant` approves; otherwise the command
exits 4 with `capability approval required`. The approval is recorded in
the profile's `grants.toml` for the plugin's exact content (subject
`plugin:<id>@sha256:...`), so a new version of the plugin needs approving
again before it loads. `plugins.enabled` and `plugins.disabled` hold the
lists; an id in both stays disabled.

```sh
prometheus plugins grants show weather          # granted and pending capabilities
prometheus plugins grants approve weather       # all requested, for this content
prometheus plugins grants revoke weather "secrets WEATHER_KEY"
```

A revocation reaches a running plugin at its next call.

## The manifest

```toml
manifest_version = 1
id = "weather"
version = "1.0.0"
kind = "wasm"                 # wasm, mcp or python
description = "Forecasts for the agent."
license = "MIT"
requires_prometheus = ">=0.1"
worlds = ["tool@1.0", "hook@1.0"]

[capabilities]
network = ["api.weather.example"]       # host[:port], `*.` prefix allowed
secrets = ["WEATHER_KEY"]
hooks = ["pre_tool_call"]
llm = { enabled = true }
```

Unknown fields, kinds, worlds, capability keys and hook names are refused
with an error naming them; so is a `manifest_version` newer than this
Prometheus reads and a `requires_prometheus` range that excludes it. The
capability keys are `network`, `fs_read`, `fs_write`, `secrets`, `llm`,
`tools_call`, `tools_override`, `hooks`, `prompt_section`, `commands`,
`cli_commands`, `providers`, `providers_override`, `approval_transport`,
`inject_messages`, `platform_actions`, `subprocess`, `ui`, `subagents` and
`credentials`.

## The WIT worlds

The plugin interface is a set of WIT packages in `wit/`, one per world
(`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`,
`context-engine`, `terminal-backend`, `ui-panel`, `delegation-router`) plus
`wit/host`, the host interfaces every world imports: `log`, `state` (data
under `<profile>/plugin-data/<id>/`), `http`, `secrets`, `llm`, `tools`,
`events`, `config` (`plugins.entries.<id>.*`) and `session`; `tool` (1.2)
and `hook` (1.3) worlds also import `wit/messaging` (live messages,
injection, platform actions), `tool` imports `wit/subagents` (subagents of
the calling session, with the `subagents` capability), and `hook` imports
`console` (a CLI command's streamed output). Each world
versions on its own; a component built against the current or the previous
major version of a world loads. `plugins/samples/` holds a sample plugin for
every world, built with `cargo component` (`cargo xtask plugin-samples`).

## Limits

Every call into a plugin is bounded by `plugins.wasm.call_timeout_ms`
(default 30 s, through fuel and epoch interruption) and its memory by
`plugins.wasm.max_memory_bytes` (256 MiB). A call that overruns fails with
`plugin '<id>' exceeded call timeout` and the turn goes on. A plugin's
HTTP request carries the same bound and fails with `request timed out`
when the server does not answer in time. Calls into a `terminal-backend`
plugin are the exception: the call timeout bounds only the time the plugin
spends computing, not the time it waits in host imports (an `execute`
waiting on its remote command is bounded by the command's timeout), and
each call runs on an instance of its own so `cancel` reaches a running
`execute`; such a plugin keeps cross-call data in `state`. Three traps
within a minute suspend the plugin until it is loaded again. Each profile
gets its own plugin instance; two profiles never share a plugin's state.

## Tools and hooks

A `tool` plugin's tools join the tool list of sessions started after it was
enabled, under a toolset named after the plugin. A tool named like a core
tool is refused unless the plugin holds `tools_override` for it, and a name
another plugin registered is refused naming both.

Hook callbacks run in plugin order, each bounded by
`plugins.hook_timeout_ms`; a failing callback is logged and the next one
still runs, and one that overruns is skipped for 60 seconds. `pre_tool_call`
callbacks may answer `modify` (arguments merged in and validated again),
`block` or `approve` (ask a human first); a block wins over an approval,
which wins over a modification. `pre_llm_call` context is added to the
current turn's user message only, never to the system prompt or the stored
history; a callback's context above `plugins.hook_context_max_chars` is
written to `<profile>/hook_outputs/<session>/` and replaced by its head,
tail and the file path.

The transform hooks rewrite: `transform_tool_result`'s first answer (even
empty) replaces a tool result before the model sees it and before it is
stored, and `transform_llm_output`'s first non-empty answer replaces the
turn's reply before it is delivered and stored. A plugin that registers
`transform_llm_output` with `hold_stream` (hook world 1.2) stops replies of
every session of the profile from streaming: surfaces show only that the
agent is busy, commentary between tool calls is stored hidden, and when the
hook fails or times out without a rewrite the person gets `reply withheld
by a plugin` instead of the original. `pre_verify` fires when a turn that
edited files is about to finish; `{action = "continue", message}` sends the
message back as a user message, at most `agent.max_verify_nudges` (3)
times in a row. `transform_api_error_classification` sees every failed
provider call first; its `{reason, retryable?, should_fallback?, ...}`
answer (a known failover reason such as `model_not_found`) decides the
retry and fallback. Observers (`on_stream_start|delta|end`,
`on_interim_message`, `agent_loop_stopped`, `on_skill_lifecycle`,
`pre_approval_request`, `post_approval_response`,
`on_human_input_request|resolved`, `post_gateway_admission` and the other
gateway events) cannot change anything: their answers are discarded.

Every session fires `on_session_start`, `on_turn_start`, `post_llm_call`
(after each loop iteration), `post_compaction`, `on_turn_end` (with the
first 500 characters of the reply) and `on_session_end`, and
`pre_api_request`, `post_api_request` or `api_request_error` around each
model call. `transform_terminal_output`'s first answer replaces a
foreground command's output before ANSI stripping, redaction and
truncation. `sessions delete|prune` fire `on_session_delete`, and loading a
plugin set fires `on_plugin_loaded` for each plugin. A few registry names
have no firing point in this build yet (`pre_auxiliary_call`,
`post_auxiliary_call`, `on_session_finalize`, `subagent_start|stop`,
`on_room_member_activity`, `pre_transcription` and the kanban hooks):
registering one logs a warning and `prometheus plugins doctor` names it.

## Shell hooks

`[[hooks.<event>]]` tables in `config.toml` run programs on hook events:

```toml
[[hooks.pre_tool_call]]
command = "~/bin/guard.sh --strict"   # split like a shell, run without one
matcher = "terminal"                  # tool events only: a regex on the tool name
timeout_secs = 60                     # clamped to 300
fail_closed = false                   # pre_tool_call only
```

The event's JSON (`hook_event_name`, `tool_name`, `tool_input`,
`session_id`, `cwd`, `profile`, `extra`) arrives on stdin; stdout may
answer `{action = "block", message}`, `{action = "modify", args}`,
`{action = "approve"}`, `{context}` (`pre_llm_call`) or `{action =
"continue", message}` (`pre_verify`). Exit code 2 from a `pre_tool_call`
hook blocks the call with its stdout, else its stderr, else `Blocked by
shell hook.`. Any other failure lets the call go on (logged) unless the
hook is `fail_closed`. Hooks run sandboxed, after every plugin hook, and
only once approved: a foreground `prometheus chat` asks the first time,
everything else skips an unapproved hook with a warning unless
`--accept-hooks` or `hooks.auto_accept = true`. The approval is bound to the
content of the program and of every argument naming a file (relative to
the profile home, or absolute; such arguments are passed as absolute
paths): editing `guard.py` of `python3 hooks/guard.py` needs a new
approval.
`transform_api_error_classification` is for plugins only.

```sh
prometheus plugins hooks list                       # matcher, timeout, consent; webhooks signed/UNSIGNED
prometheus plugins hooks test pre_tool_call --for-tool terminal
prometheus plugins hooks allow pre_tool_call '~/bin/guard.sh --strict'
prometheus plugins hooks allow --dir alert          # a gateway hook directory
prometheus plugins hooks revoke '~/bin/guard.sh --strict'
prometheus plugins hooks doctor                     # exit 1 on any problem
```

The messaging gateway also runs `<profile>/hooks/<name>/` directories
(`HOOK.toml` with `name`, `description` and `events`, and an executable
`handler` speaking the same protocol; a Hermes `HOOK.yaml` is translated).
A directory does nothing until `plugins hooks allow --dir <name>` approves
its handler; handlers run sandboxed and fire and forget, for gateway
events only.

## Outbound webhooks

```toml
[[hooks.outbound]]
name = "ci"
url = "https://ci.example.com/prometheus"   # http:// only to loopback
events = ["on_session_end", "post_tool_call"]
matcher = "terminal"                         # optional, tool events
secret_name = "CI_HOOK_SECRET"               # signs X-Prometheus-Signature-256
timeout_secs = 10
```

Each event is POSTed as `{hook_event_name, profile, tool_name, tool_input,
session_id, cwd, extra, delivery_id, timestamp}` with `X-Prometheus-Event`,
`X-Prometheus-Delivery` and, with a secret, `X-Prometheus-Signature-256:
sha256=<HMAC-SHA256 of the body>`. Deliveries queue (1000 per target, the
rest dropped with a warning) so a slow endpoint never slows a turn; a
connection error or 5xx is retried once, redirects are not followed, the
egress policy applies, and safe mode sends nothing. A one-shot chat waits
up to 5 seconds at exit for its queued deliveries.

## What else plugins contribute

- **System-prompt sections.** A plugin granted `prompt_section` registers
  sections by id (refused when another plugin took the id); each is
  rendered once when a session's prompt is built and stays as it is for
  that session. A section larger than its grant or 4000 characters is
  dropped, and so are sections past 8000 characters in total.
- **Slash commands** listed in its `commands` grant, on every surface the
  command names. A name a built-in command or alias already has is refused
  and the built-in keeps it.
- **Skills** under its `skills/` directory, offered as `<plugin>:<skill>`
  while the plugin is enabled, read in place (never copied into the
  profile).
- **Events.** `events.emit("updated", ...)` publishes `<plugin>:updated`
  (a name with `:`, such as `prometheus:session_start`, is refused);
  subscribers receive events in order, later, and a failing subscriber
  never affects the emitter.
- **Model calls** (`llm` capability) run on the session's model and are
  charged to the session under `plugin.<id>`, counting toward its budgets;
  naming a model needs `allow_model_override` and the model in
  `allowed_models`.
- **Providers.** A config key that names a provider (`web.search_backend`,
  `web.extract_backend`, `image_gen.provider`...) is served by a built-in
  of that name or by an enabled plugin registering it; otherwise the
  feature is unavailable with `<category> provider '<name>' is not
  installed or not enabled`, never silently another provider.

- **Commands for the command line** in its `cli_commands` grant:
  `prometheus plugins run <id> <command> [args...]` runs one (its stdout
  and stderr streamed as the plugin writes them, then its exit code; 70
  when the plugin crashes) and `prometheus plugins
  run <id>` lists them; `prometheus help` lists them under `plugin
  commands`.
- **An approval transport** (`approval_transport`): with
  `plugins.approval_transport = "<plugin>:<name>"` approval requests go to
  the plugin instead of the session's surface. It sees the redacted request
  and the choices it may answer; an answer for another request, or a choice
  the request does not offer (`always` on a once-only request), is denied;
  so are errors, timeouts and a missing transport, unless
  `plugins.approval_transport_fallback = "builtin"` asks on the surface.
- **Live messages, injection and platform actions** (daemon sessions): a
  plugin posts and edits a message in the calling session's chat, a
  plugin granted `inject_messages` queues text into a conversation of its
  own profile (never another profile's), and one granted
  `platform_actions` adds reactions or sets thread titles; every call
  answers what happened instead of failing.
- **Model providers** (`providers`): a provider plugin's descriptors join
  the profile's provider registry (sessions, `prometheus model` and
  `prometheus auth`), with its header and request hooks; `model list` and
  the model pickers ask its model-listing hook (a failing one is reported
  and the catalog answers). An id of a built-in provider is refused (the
  built-in stays) unless the plugin holds `providers_override` for it.

Plugins load in `requires_plugins` order: a plugin whose dependency is
missing, disabled or failed is refused with `missing dependency '<id>'`,
plugins on a dependency cycle are refused naming the cycle, and a plugin
that takes longer than `plugins.load_timeout_ms` (10 s) to load is skipped
with `load timed out` while the others load.

## Reloading

```sh
prometheus plugins reload [weather]   # in the running daemon, when there is one
```

and `/plugins reload [<id>] [--now]` in a session replace the plugin
instances: hooks, slash commands and event subscribers change at once in
running sessions; tools, system-prompt sections and skills reach sessions
started afterwards, so a running session keeps its cached prompt. The
command prints which parts are active now and which wait (`tools: next
session`). `--now` also rebuilds the current session's tools and prompt,
recorded as `prompt.rebuilt` with reason `plugins --now`; skills still wait
for the next session (`skills: next session`), because a session's skill
directories and the sandbox reads granted for them are fixed when it
starts. `/plugins` alone
lists the plugins with their state and load error.

`plugins install`, `enable`, `disable`, `update`, `remove`, `adopt` and
`grants approve|revoke` reload a running daemon's plugins the same way when
they change something, and print the same report after `the running daemon
reloaded its plugins:`. Without a running daemon the next session loads
the change; a session already open in its own process (`chat
--in-process`, `--safe-mode`) picks it up at its next `/plugins reload`,
which reads `config.toml` again with the session's own `--set` overrides
(and `--ignore-user-config` / `--safe-mode`) kept. If the daemon cannot
reload, the command still succeeds (the change is made) and warns;
`prometheus plugins reload` retries.

## Safe mode

`prometheus --safe-mode chat --model <m> --provider <p>` starts with no
plugins, MCP servers, sidecars, hooks or webhooks, ignores the profile's
`config.toml` (it starts even when the file does not parse) and its rules,
memory and identity file, and runs in its own process on the line
interface rather than in the daemon. It shows a banner, changes no setting, and needs `--model` and
`--provider`, exiting 78 without them. `plugins.safe_mode = true` skips the
plugins alone.

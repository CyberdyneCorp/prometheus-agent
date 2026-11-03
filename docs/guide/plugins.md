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
`inject_messages`, `platform_actions`, `subprocess` and `ui`.

## The WIT worlds

The plugin interface is a set of WIT packages in `wit/`, one per world
(`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`,
`context-engine`, `terminal-backend`, `ui-panel`, `delegation-router`) plus
`wit/host`, the host interfaces every world imports: `log`, `state` (data
under `<profile>/plugin-data/<id>/`), `http`, `secrets`, `llm`, `tools`,
`events`, `config` (`plugins.entries.<id>.*`) and `session`. Each world
versions on its own; a component built against the current or the previous
major version of a world loads. `plugins/samples/` holds a sample plugin for
every world, built with `cargo component` (`cargo xtask plugin-samples`).

## Limits

Every call into a plugin is bounded by `plugins.wasm.call_timeout_ms`
(default 30 s, through fuel and epoch interruption) and its memory by
`plugins.wasm.max_memory_bytes` (256 MiB). A call that overruns fails with
`plugin '<id>' exceeded call timeout` and the turn goes on. A plugin's
HTTP request carries the same bound and fails with `request timed out`
when the server does not answer in time. Three traps
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
recorded as `prompt.rebuilt` with reason `plugins --now`. `/plugins` alone
lists the plugins with their state and load error.

`plugins install`, `enable`, `disable`, `update`, `remove`, `adopt` and
`grants approve|revoke` reload a running daemon's plugins the same way when
they change something, and print the same report after `the running daemon
reloaded its plugins:`. Without a running daemon no session runs, and the
next one loads the change. If the daemon cannot reload, the command still
succeeds (the change is made) and warns; `prometheus plugins reload` retries.

## Safe mode

`prometheus --safe-mode chat --model <m> --provider <p>` starts with no
plugins, MCP servers, sidecars, hooks or webhooks, ignores the profile's
`config.toml` (it starts even when the file does not parse) and its rules,
memory and identity file, and runs in its own process on the line
interface rather than in the daemon. It shows a banner, changes no setting, and needs `--model` and
`--provider`, exiting 78 without them. `plugins.safe_mode = true` skips the
plugins alone.

# Plugins

Plugins add capability at the edges: tools for the model, hook callbacks
around tool and model calls, and (as their hosts land) providers, platform
adapters, memory providers, context engines, terminal backends, UI panels
and delegation routers. A native plugin is a WebAssembly component that
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
`plugin:<id>@sha256:...`), so editing any file of the plugin means approving
it again before it loads. `plugins.enabled` and `plugins.disabled` hold the
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

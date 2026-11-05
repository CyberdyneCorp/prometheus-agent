# The python bridge

The python bridge runs Hermes-style Python plugins (a directory with
`plugin.yaml` and a `register(ctx)` entry, model-provider plugins calling
`register_provider(ProviderProfile(...))`, provider objects) and Python
skill scripts on a managed interpreter, out of the Prometheus process. Each
plugin gets its own sidecar per profile: a sandboxed process speaking
length-prefixed JSON-RPC over stdin/stdout, with a shim of the documented
Hermes `PluginContext` API. A crash, a leak or a stalled interpreter costs
the plugin, never the agent.

## The managed interpreter

Plugins run only on a managed CPython, never on a system Python. Install it
once (it needs the network; uv, pinned in this binary, provisions it and
checks every download against its hash manifest):

```text
$ prometheus runtimes install python@3.12
```

`plugins install` and `plugins update` of a Python plugin install it
when it is missing (unless `runtimes.allow_lazy_install = false` or
`runtimes.exclude` names `cpython` or `uv`); a session never downloads it.
`python_bridge.python_version` picks `3.11`, `3.12` (default) or `3.13`.
Without the interpreter every bridged plugin fails with
`Python runtime 3.12 is not installed and could not be downloaded; run
prometheus runtimes install python@3.12`, and the rest of Prometheus keeps
working.

## Installing a plugin

A Hermes plugin installs like any plugin. Every Python plugin is audited
first: `prometheus plugins validate <dir>` prints the same report. The
audit lists each `ctx.<member>` outside the supported subset, each import of
a Hermes internal module and each direct `os.environ` read of a secret,
with file and line, and classifies the plugin:

- `compatible`: nothing found;
- `degraded`: findings outside the load path (a handler calling an
  unsupported API raises `UnsupportedPluginApi` when it runs);
- `incompatible`: an unsupported API runs while the plugin loads
  (`register`, or an import); it installs only with `--force` and stays
  disabled.

A `plugin.yaml` declares no capabilities, so the plugin asks for what the
audit infers: the secrets it reads (`requires_env`, `get_secret`,
`os.environ`), the hosts its URLs name, its hooks, commands and providers.
You approve them on enable, as for any plugin.

Requirements (`requirements.txt`, `pyproject.toml`
`[project].dependencies` or `pip_dependencies`) must have an upper bound
(`requests>=2.31,<3`) unless `python_bridge.allow_unbounded_requirements =
true`. `plugins install` and `plugins update` resolve them into
`python/locks/<id>/bridge.lock` with exact versions and hashes, and build
the plugin's environment `python/envs/<id>-<digest>/`; this is the only
time the bridge uses the network. Packages install from wheels only: a
requirement published only as a source distribution is refused by name,
because building it would run its `setup.py` outside the plugin's sandbox.
A session reuses the environment, or rebuilds it offline from the package
cache.

`prometheus plugins show <id>` prints the Hermes API level the shim
implements, the interpreter and the environment.

## What a plugin may do

The shim implements the documented `PluginContext` subset (tools, hooks,
slash and CLI commands, system-prompt sections, providers, approval
transports, live messages, injection, platform actions, events, `llm`,
`state`, plugin settings) with the same grants as a WASM plugin. Anything
else raises `UnsupportedPluginApi`: `ctx.register_platform() is not
supported by the Prometheus python bridge: ...` names the reason and the
alternative. Imports of `agent.memory_provider`, `providers.base` and the
other documented modules resolve to the shim; any other `hermes_cli`,
`agent`, `gateway` or `tools` module raises `ImportError` by name.

A sidecar runs under the OS sandbox: it reads its plugin directory, its
environment and the interpreter, writes `plugin-data/<id>/` and a private
temporary directory, reaches the network only through a loopback egress
proxy admitting its granted hosts, and starts processes only with
`subprocess` granted. Its environment holds `PATH`, `HOME` (its data
directory), `LANG`, `TMPDIR`, `PYTHONNOUSERSITE` and its granted secrets,
nothing else.

## Model-provider plugins

A Hermes model-provider plugin becomes a provider: its API mode picks the
wire format, `env_vars` the secret names and its base URL the key hosts.
`providers.base.ProviderProfile` mirrors Hermes field for field (with
`LazyHeaders`, `OMIT_TEMPERATURE` and `hermes_cli.version_info.hermes_user_agent`),
so the plugins Hermes ships, such as `nvidia`, `gmi` or `xiaomi`, load
unchanged. Prometheus sends and streams every request itself; the
profile's hooks run in the sidecar: headers (`client_headers` or
`default_headers`, `request_headers`, `extra_headers` extras), the body
(`prepare_messages`, `default_max_tokens` / `get_max_tokens` when the
request sets no cap, `fixed_temperature`, `build_extra_body`,
`build_api_kwargs_extras`), model listing and error classification. A hook
that crashes or times out fails the call and the fallback chain applies.
Replacing a built-in provider id needs the `providers_override` grant; the
replacement keeps the built-in's aliases.

A memory provider's `cli.py` `register_cli(subparser)` becomes the command
named after the plugin (`prometheus plugins run <id> <id> ...`). It is
available only while that provider is the active `memory.provider`; until
external memory providers arrive every profile runs the built-in store,
so it is refused with `command '<id>' of memory provider '<id>' is
available only while '<id>' is the active memory.provider (now 'builtin')`.

## Lifecycle and limits

A plugin with hooks, providers or computed sections starts with the
profile's first session; one with only tools or commands starts on first
use and stops after `python_bridge.idle_shutdown_secs` (600) without calls.
Each call is bounded by `python_bridge.call_timeout_ms` (hooks by
`plugins.hook_timeout_ms`): `bridge call timed out after <n> ms`. The host
pings every sidecar every 15 seconds and kills one that misses three pings
(a GIL stall). A sidecar whose tree goes above `python_bridge.max_memory_bytes`
or `python_bridge.max_processes` is killed with `resource limit exceeded
(<limit>)`. The OS enforces what it can: on Linux each sidecar process has a
data-segment rlimit of `max_memory_bytes` and the sidecar is the subreaper
of its descendants; on Windows the sandbox's Job Object caps the tree's
committed memory and process count. macOS has no usable equivalent, so
there the host's sampler (twice a second) is the bound and usage can
overshoot for one sample. On every OS the sampler checks the tree's
totals and remembers its members, so a child that left the sidecar's
process group still dies with it. `prometheus doctor plugins` names the
mechanism and how strongly direct sockets are denied.
After a crash, calls in flight fail with `bridge_crashed: plugin '<id>'
exited (<status>)` and the sidecar restarts after 1 s, doubling to 60 s;
five crashes within ten minutes mark the plugin `failed` until
`prometheus bridge restart <id>`.

## Diagnostics

```console
$ prometheus bridge status
no bridge sidecars are running for this profile
$ prometheus bridge env "Not a name" # exits 2
error: invalid environment name 'Not a name' (expected ^[a-z0-9][a-z0-9_-]{0,63}$)
```

- `prometheus bridge status [--json]`: every sidecar with its pid, state
  (`starting`, `running`, `idle`, `crashed`, `failed`), uptime, restarts
  and memory.
- `prometheus bridge logs <id> [--follow]`: the plugin's stderr and
  lifecycle notes (`logs/bridge/<id>.log`, rotated at 10 MiB, 3 files).
- `prometheus bridge restart <id>`: clears a `failed` plugin.
- `prometheus bridge doctor`: the interpreter, each environment against
  its `bridge.lock` (naming a package that no longer matches) and the
  sandbox enforcement level; exit 1 when a check fails.

## Skill scripts and side environments

A skill declaring `metadata.prometheus.python` gets its own pinned
environment (`python/skills/<skill>-<digest>/`). A [`terminal`](../reference/tools.md#terminal)
command that runs a file of that skill finds `python`, `python3` and `pip`
there, under the session's sandbox; plugin environments are never on its
path. A skill without the declaration gets the managed interpreter with no
extra packages.

```yaml
metadata:
  prometheus:
    python:
      requires_python: ">=3.11"
      requirements:
        - pypdf>=4,<5
```

A script can also ask for a named environment; stdout is only the
interpreter's path:

```text
$ "$(prometheus bridge env pdf --extra 'pypdf>=4,<5')" extract.py
```

## Migrating from Hermes

The Hermes migration (`prometheus migrate from-hermes`, which arrives with
the hermes-migration work) imports plugins through the bridge: it copies
each Hermes plugin into the profile as a python plugin, audits it, keeps it enabled only when Hermes
had it enabled and it is not `incompatible`, and proposes the capabilities
it needs for approval at first enable. The migration report lists
incompatible plugins with the APIs that block them.

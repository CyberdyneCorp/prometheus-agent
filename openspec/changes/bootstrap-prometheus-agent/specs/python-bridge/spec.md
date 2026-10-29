# Python Bridge

## Purpose
The python bridge runs existing Hermes-style Python plugins (`plugin.yaml` + `register(ctx)`, memory providers, context engines, provider objects) and Python skill scripts against a managed, version-pinned CPython, out of the Prometheus process. Each plugin runs in its own sandboxed sidecar speaking versioned JSON-RPC over stdio and receives a compatibility shim implementing a declared subset of the Hermes `PluginContext` API. The key decisions are out-of-process isolation (a crash, leak or GIL stall cannot take down the agent), capability grants enforced at the process boundary, and explicit, named errors for every Hermes API the bridge does not support.

## ADDED Requirements

### Requirement: Scope of hosted code
The bridge SHALL host exactly these kinds of Python code: general Hermes plugins (directory with `plugin.yaml` and a `register(ctx)` entry), Hermes memory providers (`MemoryProvider` subclasses), Hermes context engines (`ContextEngine` subclasses), Hermes provider-object plugins (image-gen, video-gen, web-search, TTS, transcription, secret-source), and the per-skill Python environments used by skill scripts; it SHALL NOT host Python code for any other purpose, and Prometheus SHALL never import Python into its own process.
**Milestone:** M2

#### Scenario: Memory provider runs in a sidecar
- **GIVEN** a Hermes memory provider plugin `honcho` installed and `memory.provider = "honcho"`
- **WHEN** a session starts
- **THEN** the provider SHALL run inside a bridge sidecar and the Prometheus process SHALL contain no Python interpreter

### Requirement: Managed CPython runtime
The bridge SHALL run plugins only on a managed CPython installed lazily into the shared managed-runtime store from a hash-pinned manifest, with the interpreter version chosen by `python_bridge.python_version` (default `"3.12"`, allowed `"3.11"`, `"3.12"`, `"3.13"`), SHALL never use a system Python, and when the runtime is missing and cannot be downloaded SHALL fail every bridged plugin with `Python runtime <version> is not installed and could not be downloaded; run prometheus runtimes install python@<version>` while the rest of Prometheus keeps working.
**Milestone:** M2

#### Scenario: Offline first use fails clearly
- **GIVEN** no managed Python installed and no network
- **WHEN** a bridged plugin is enabled and a session starts
- **THEN** the plugin SHALL be reported failed with the install hint and the session SHALL start without it

### Requirement: Per-plugin pinned environments
The bridge SHALL create one virtual environment per plugin at `<profile_home>/python/envs/<plugin_id>-<lock_digest12>/` from the plugin's `requirements.txt` or `pyproject.toml` resolved into a lock file `bridge.lock` with exact versions and hashes, SHALL refuse requirements lacking an upper bound unless `python_bridge.allow_unbounded_requirements = true`, SHALL perform network access only during `prometheus plugins install`/`update` (never at session start), and SHALL rebuild the environment only when the lock digest or interpreter version changes.
**Milestone:** M2

#### Scenario: Environment is reused
- **GIVEN** an installed bridged plugin with an unchanged lock file
- **WHEN** a second session starts
- **THEN** the bridge SHALL reuse the existing environment without network access

#### Scenario: Plugin requirements are pinned with hashes
- **WHEN** a bridged plugin with `requests>=2.31,<3` is installed
- **THEN** `bridge.lock` SHALL record one exact `requests` version and its hash

### Requirement: Hermes API compatibility level
The bridge SHALL ship a shim package declaring `HERMES_COMPAT_LEVEL` (the Hermes release whose documented plugin API it implements) and SHALL refuse a plugin whose `plugin.yaml` `requires_hermes` range excludes that level with `requires Hermes <range>; bridge implements <level>`; `prometheus plugins show <id>` SHALL display the compat level, interpreter version and environment path.
**Milestone:** M2

#### Scenario: Plugin needing a newer Hermes API
- **GIVEN** a plugin with `requires_hermes: ">=2027.1"` and a bridge compat level `2026.10`
- **WHEN** it is loaded
- **THEN** the system SHALL refuse it with `requires Hermes >=2027.1; bridge implements 2026.10`

### Requirement: Sidecar wire protocol
Host and sidecar SHALL communicate with JSON-RPC 2.0 messages framed by a 4-byte big-endian length prefix over the sidecar's stdin and stdout (stderr reserved for logs), with a maximum frame size of 16 MiB (larger frames rejected with error `frame_too_large`), starting with a `bridge.hello` handshake exchanging `protocol_version` (currently 1), `compat_level` and the plugin id; a protocol version mismatch SHALL terminate the sidecar with `bridge protocol mismatch (host <a>, sidecar <b>)`, and the method set SHALL be closed: `bridge.hello`, `bridge.ping`, `bridge.shutdown`, `plugin.load`, `plugin.unload`, `tool.invoke`, `hook.invoke`, `command.invoke`, `provider.call`, `event.deliver` (host to sidecar) and `host.call` (sidecar to host), with unknown methods answered by JSON-RPC error -32601.
**Milestone:** M2

#### Scenario: Oversized frame is rejected
- **WHEN** a plugin returns a 20 MiB tool result
- **THEN** the host SHALL reject the frame with `frame_too_large` and the tool call SHALL fail with that error

### Requirement: Supported PluginContext subset
The shim `PluginContext` SHALL implement `register_tool`, `register_hook`, `register_middleware`, `register_command`, `register_skill`, `register_system_prompt_section`, `register_memory_provider`, `register_context_engine`, `register_context_reference`, `register_image_gen_provider`, `register_video_gen_provider`, `register_web_search_provider`, `register_tts_provider`, `register_transcription_provider`, `register_secret_source`, `register_auxiliary_task`, `register_redaction_patterns`, `register_locale_dir`, `register_automation_blueprint`, `emit`, `subscribe`, `dispatch_tool`, `llm` (`complete`, `complete_structured`, `acomplete`, `acomplete_structured`), `state`, `get_config`, `set_config`, `profile_name`, `plugin_id`, `has_plugin`, `has_capability`, `session_cwd`, `session_surface`, `on_unload` and `spawn_task`, each mapped to the equivalent plugin-system host interface and subject to the plugin's capability grants.
**Milestone:** M2

#### Scenario: Hermes tool plugin works unchanged
- **GIVEN** a Hermes plugin whose `register(ctx)` calls `ctx.register_tool(name="spotify_play", schema=..., handler=...)`
- **WHEN** the plugin is loaded and the model calls `spotify_play`
- **THEN** the handler SHALL run in the sidecar and its return value SHALL be the tool result

### Requirement: Unsupported APIs fail with a named error
Calling a Hermes `PluginContext` method or attribute outside the supported subset (including `register_platform`, `register_platform_handler`, `register_telegram_handler`, `register_slack_action_handler`, `register_dashboard_auth_provider`, `register_approval_transport`, `register_login_backend`, `register_browser_page_backend`, `post_message`, `apost_message`, `inject_gateway_message` and `inject_tui_message`) SHALL raise `UnsupportedPluginApi` with the message `ctx.<name>() is not supported by the Prometheus python bridge: <reason>`, where the reason states why and the Prometheus alternative when one exists; `register_cli_command` SHALL be skipped with a WARN and the plugin SHALL continue loading.
**Milestone:** M2

#### Scenario: Platform adapter plugin gets a clear error
- **GIVEN** a Hermes plugin whose `register(ctx)` calls `ctx.register_platform(...)`
- **WHEN** it is loaded
- **THEN** loading SHALL fail with `ctx.register_platform() is not supported by the Prometheus python bridge:` followed by a reason pointing to the `platform-adapter` WASM world, and `prometheus plugins list` SHALL show that message

### Requirement: Static compatibility audit
`prometheus plugins validate <path>` and every install of a Python plugin SHALL statically analyse its source for `ctx.<method>` uses outside the supported subset, imports of Hermes internal modules not provided by the shim, and direct `os.environ` reads of secret names, and SHALL print a report classifying the plugin as `compatible`, `degraded` (warnings only) or `incompatible` (an unsupported API reachable from `register`), installing an `incompatible` plugin only with `--force` and leaving it disabled.
**Milestone:** M2

#### Scenario: Audit flags an unsupported call before install
- **GIVEN** a plugin source that calls `ctx.post_message` from a tool handler
- **WHEN** the user runs `prometheus plugins validate ./plugin`
- **THEN** the report SHALL list `post_message` with file and line and classify the plugin `degraded`

### Requirement: Hermes import shims
The shim SHALL make importable, at their documented Hermes paths, the plugin-facing ABCs and helpers (`agent.memory_provider.MemoryProvider`, `agent.context_engine.ContextEngine`, the image-gen, video-gen, web-search, TTS, transcription and secret-source provider bases, `agent.secret_scope.get_secret` and `agent.memory_provider.spawn_context_thread`), and an import of any other `hermes_cli`, `agent`, `gateway`, `tools` or `run_agent` module SHALL raise `ImportError` naming the module and stating that only the documented plugin API is available.
**Milestone:** M2

#### Scenario: Internal import is refused by name
- **WHEN** a plugin executes `from gateway.run import GatewayRunner`
- **THEN** it SHALL raise `ImportError` whose message names `gateway.run` and the documented-API rule

### Requirement: Hook name and payload translation
The bridge SHALL map Hermes hook names to the plugin-system hook registry one-to-one where the semantics match, SHALL pass each callback only the keyword arguments its signature declares (all fields to `**kwargs` callbacks) plus `telemetry_schema_version="hermes.observer.v1"`, SHALL deliver placeholders in place of live handles that cannot cross a process boundary (for `pre_gateway_dispatch`, the gateway runner and session store) and record the plugin as `degraded`, and SHALL refuse at load a Hermes hook name with no Prometheus equivalent naming it.
**Milestone:** M2

#### Scenario: Narrow signature gets narrow payload
- **GIVEN** a `post_tool_call` callback declared as `def cb(tool_name, result)`
- **WHEN** a tool completes
- **THEN** the callback SHALL be called with only `tool_name` and `result`

### Requirement: Typed values across the wire
The bridge SHALL rebuild arguments and return values crossing the boundary according to the type annotations of the Hermes ABC method being called (dataclasses, enums, `pathlib.Path`, lists and optionals), SHALL pass unannotated values as JSON data, and SHALL fail a call whose return value cannot be serialized with `unserializable return value of type <T> from <method>`.
**Milestone:** M2

#### Scenario: Provider result keeps its declared type
- **GIVEN** a web-search provider whose `search` is annotated to return a list of result dataclasses
- **WHEN** Prometheus calls it through the bridge
- **THEN** the host SHALL receive structured results with every declared field present

### Requirement: Capability grants enforced at the process boundary
The bridge SHALL launch each sidecar under the OS sandbox profile derived from the plugin's granted capabilities: filesystem read-only access to its plugin directory, environment and the managed runtime, read-write access to `<profile_home>/plugin-data/<id>/` and a private temporary directory, plus any granted `fs_read`/`fs_write` paths; outbound network only through the host egress proxy restricted to the granted `network` hosts (direct sockets denied where the OS sandbox supports it, with the enforcement level reported by `prometheus doctor`); and no child processes unless `subprocess = true` is granted.
**Milestone:** M2

#### Scenario: Plugin cannot read the user's SSH keys
- **GIVEN** a bridged plugin with no `fs_read` grant
- **WHEN** it opens `~/.ssh/id_ed25519`
- **THEN** the open SHALL fail with a permission error inside the sidecar

### Requirement: Secrets and environment isolation
The sidecar's process environment SHALL contain only `PATH`, `HOME` (set to its private data directory), `LANG`, `TMPDIR`, `PYTHONNOUSERSITE=1` and the secrets named in the plugin's granted `secrets` capability, and the shim's `get_secret(name)` SHALL return only granted secrets and raise `PermissionError` naming the secret otherwise; config access through `get_config`/`set_config` SHALL be limited to `plugins.entries.<id>.*`.
**Milestone:** M2

#### Scenario: Ungranted secret is invisible
- **GIVEN** a bridged plugin granted only `SPOTIFY_TOKEN`
- **WHEN** it reads `os.environ.get("OPENAI_API_KEY")`
- **THEN** the value SHALL be `None`

### Requirement: One profile per sidecar
Each sidecar SHALL serve exactly one plugin for exactly one profile, started with that profile's ProfileScope, and the bridge SHALL never route a call from one profile to a sidecar of another; a gateway serving several profiles that enable the same plugin SHALL run one sidecar per profile.
**Milestone:** M2

#### Scenario: Shared gateway, separate sidecars
- **GIVEN** plugin `notes` enabled in profiles `a` and `b` on one gateway
- **WHEN** both profiles use it
- **THEN** two sidecar processes SHALL exist, each with its own data directory and granted secrets

### Requirement: Sidecar lifecycle
The bridge SHALL start a plugin's sidecar when the first session of its profile starts if the plugin registers hooks, providers or prompt sections, and otherwise on the first call to one of its tools or commands; `plugin.load` (import plus `register`) SHALL be bounded by `plugins.load_timeout_ms`; an idle sidecar with no hooks or providers SHALL be stopped after `python_bridge.idle_shutdown_secs` (default 600, 0 disables); shutdown SHALL send `bridge.shutdown`, run `on_unload` callbacks, wait 5 seconds, then terminate the process tree, escalating to a forced kill after 2 more seconds.
**Milestone:** M2

#### Scenario: Process tree is reaped
- **GIVEN** a bridged plugin granted `subprocess = true` that spawned a long-running child
- **WHEN** Prometheus shuts down
- **THEN** both the sidecar and its child SHALL be terminated within 7 seconds

### Requirement: Call timeouts and stall detection
Each call into a sidecar SHALL be bounded by `python_bridge.call_timeout_ms` (default 30000; hook calls use `plugins.hook_timeout_ms`), returning `bridge call timed out after <n> ms` on expiry; the host SHALL send `bridge.ping` every 15 seconds while the sidecar is running, and 3 consecutive unanswered pings (a hung interpreter or GIL stall) SHALL cause the sidecar to be killed and handled as a crash.
**Milestone:** M2

#### Scenario: GIL stall is recovered
- **GIVEN** a plugin that enters a native loop holding the GIL
- **WHEN** 3 pings go unanswered
- **THEN** the host SHALL kill the sidecar, fail its in-flight calls with `bridge_crashed`, and schedule a restart

### Requirement: Crash isolation and restart policy
When a sidecar exits unexpectedly the host SHALL fail its in-flight calls with `bridge_crashed: plugin '<id>' exited (<status>)`, keep the agent and other sidecars running, write the last 200 stderr lines to the plugin log, and restart the sidecar with exponential backoff starting at 1 second, doubling up to 60 seconds; after 5 crashes within 10 minutes the plugin SHALL be marked `failed` (its tools returning `plugin '<id>' is failed; see prometheus bridge logs <id>`) until `prometheus bridge restart <id>` or the next gateway start.
**Milestone:** M2

#### Scenario: Crash loop is contained
- **GIVEN** a plugin that segfaults on every tool call
- **WHEN** it crashes 5 times within 10 minutes
- **THEN** the plugin SHALL be marked `failed`, no further restarts SHALL happen, and the session SHALL continue with its other tools

### Requirement: Resource limits
Each sidecar SHALL be limited to `python_bridge.max_memory_mib` (default 1024) of resident memory and `python_bridge.max_processes` (default 16) processes, using OS facilities (cgroups on Linux, Job Objects on Windows, rlimits on macOS), and exceeding a limit SHALL be handled as a crash with the status `resource limit exceeded (<limit>)`.
**Milestone:** M2

#### Scenario: Memory leak is bounded
- **GIVEN** a plugin that allocates without bound
- **WHEN** its resident memory exceeds 1024 MiB
- **THEN** the sidecar SHALL be terminated and the crash reason SHALL name the memory limit

### Requirement: Registrations follow plugin-system reload rules
Registrations received from a sidecar SHALL be applied with the same rules as WASM plugins: tools and system-prompt sections join only sessions started afterwards, hooks and commands become active immediately, and a sidecar restart after a crash SHALL re-run `register` and SHALL NOT change the tool list or prompt of any session already in progress (a tool whose registration disappears after restart SHALL return `tool '<name>' is no longer provided by plugin '<id>'`).
**Milestone:** M2

#### Scenario: Restart does not mutate a live session
- **GIVEN** an active session using tools of a bridged plugin
- **WHEN** the sidecar crashes and restarts
- **THEN** the session's tool schemas and system prompt SHALL be byte-identical to before the crash

### Requirement: Python skill scripts
For a skill declaring `metadata.prometheus.python` (`requires_python`, `requirements`) the bridge SHALL build a per-skill environment at `<profile_home>/python/skills/<skill>-<lock_digest12>/` using the same pinning rules as plugins, and the execution environments SHALL resolve `python`, `python3` and `pip` inside terminal and code-execution calls for that skill to this environment while running under the session's sandbox (not under a bridge sidecar); a skill without that declaration SHALL get the managed interpreter with no extra packages.
**Milestone:** M2

#### Scenario: Skill script has no access to plugin environments
- **GIVEN** skill `pdf-tools` with its own environment and plugin `notes` with another
- **WHEN** a `pdf-tools` script runs `import notes_sdk`
- **THEN** the import SHALL fail because plugin environments are not on the skill's path

### Requirement: Bridge diagnostics
The system SHALL provide `prometheus bridge status` (per sidecar: plugin, profile, pid, state `starting|running|idle|crashed|failed`, uptime, restarts, memory), `prometheus bridge logs <id> [--follow]` reading `<profile_home>/logs/bridge/<id>.log` (rotated at 10 MiB, 3 files kept), `prometheus bridge restart <id>`, and `prometheus bridge doctor` checking the managed runtime, each environment's lock integrity and the sandbox enforcement level, exiting 1 when any check fails.
**Milestone:** M2

#### Scenario: Doctor detects a corrupted environment
- **GIVEN** a plugin environment whose installed packages no longer match `bridge.lock`
- **WHEN** the user runs `prometheus bridge doctor`
- **THEN** the report SHALL name the plugin and the mismatched package and the command SHALL exit 1

### Requirement: Migration of Hermes plugins
When `hermes-migration` imports a Hermes home, the system SHALL copy each user plugin directory into `<profile_home>/plugins/<id>/` as kind `python`, run the static compatibility audit, preserve the Hermes enabled state only for plugins classified `compatible` or `degraded`, propose capability grants inferred from the audit for the user to approve on first enable, and list `incompatible` plugins with their blocking API names in the migration report.
**Milestone:** M2

#### Scenario: Incompatible plugin is imported disabled
- **GIVEN** an enabled Hermes plugin that calls `ctx.register_platform`
- **WHEN** `prometheus migrate from-hermes` runs
- **THEN** the plugin SHALL be copied but left disabled and the report SHALL cite `register_platform`

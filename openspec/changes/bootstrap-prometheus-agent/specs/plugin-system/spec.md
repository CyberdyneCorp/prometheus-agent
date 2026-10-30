# Plugin System

## Purpose
The plugin system is how Prometheus grows capability at the edges instead of in the core. Native plugins are WebAssembly components that implement typed WIT worlds (`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`, `context-engine`, `terminal-backend`, `ui-panel`) and run in the host with only the capabilities the user granted; external tools arrive through MCP servers, and Hermes-style Python plugins run out-of-process through the python bridge. The key decisions are that no third-party code runs in-process with ambient authority, that every registry (hooks, commands, worlds, capabilities) is typed so unknown names fail at load, and that hot reload never mutates a live session's cached prompt or tool list.

## ADDED Requirements

### Requirement: Plugin kinds and manifest
Every plugin SHALL be a directory containing `plugin.toml` with `manifest_version` (integer, currently 1), `id` (matching `^[a-z0-9][a-z0-9_-]{0,63}$`), `version` (semver), `kind` (`wasm`, `mcp` or `python`), `description`, `license`, `requires_prometheus` (semver range), `worlds` (list of WIT world names with versions, for `wasm`), `capabilities` (table), optional `requires_plugins` and optional `[[kanban_lanes]]` entries (fields and semantics owned by `kanban` › Worker lanes); for `python` kind a Hermes `plugin.yaml` SHALL be accepted in place of `plugin.toml` and translated by the python bridge. A manifest with an unknown kind, world, capability key or top-level field, or with a `manifest_version` greater than supported, SHALL be refused with an error naming the field, and a `requires_prometheus` range excluding the running version SHALL be refused before instantiation.
**Milestone:** M2

#### Scenario: Unknown capability key fails closed
- **GIVEN** a `plugin.toml` with `[capabilities] netwrok = ["api.example.com"]`
- **WHEN** the plugin is loaded
- **THEN** the system SHALL refuse it with `unknown capability 'netwrok'` and `prometheus plugins list` SHALL show the error

#### Scenario: Newer manifest is refused
- **GIVEN** a manifest with `manifest_version = 2` on a host supporting 1
- **WHEN** discovery runs
- **THEN** the system SHALL refuse the plugin with `requires a newer Prometheus (manifest_version 2)`

### Requirement: Discovery sources and identity
The system SHALL discover plugins from first-party plugins embedded in the binary, from `<profile_home>/plugins/<id>/`, and from `<root>/.prometheus/plugins/<id>/` only when the project root is listed in `plugins.trusted_project_dirs`; a non-embedded plugin whose `id` equals an embedded plugin's id SHALL be refused with `plugin id '<id>' is reserved by a built-in plugin`, and two non-embedded plugins with the same id SHALL be resolved project over user with the shadowed copy reported by `prometheus plugins list`.
**Milestone:** M2

#### Scenario: Untrusted project plugin is not scanned
- **GIVEN** `./.prometheus/plugins/x/` in a repository not listed in `plugins.trusted_project_dirs`
- **WHEN** a session starts there
- **THEN** the system SHALL NOT read or instantiate `x`

### Requirement: Opt-in enablement
The system SHALL instantiate a non-embedded plugin only when its id is in `plugins.enabled` and not in `plugins.disabled`; a plugin listed in both SHALL stay disabled and be reported with `listed in both plugins.enabled and plugins.disabled; disabled wins`; `prometheus plugins enable <id>` SHALL add the id to `plugins.enabled` and remove it from `plugins.disabled`, and `disable` SHALL do the reverse.
**Milestone:** M2

#### Scenario: Installed is not enabled
- **GIVEN** a plugin installed with `--no-enable`
- **WHEN** a session starts
- **THEN** the plugin SHALL NOT be instantiated and `prometheus plugins list` SHALL show it as `disabled`

### Requirement: Capability declarations
A plugin SHALL declare every authority it needs in `[capabilities]` using only these keys: `network` (list of `host[:port]` patterns, `*.` prefix allowed, no bare `*`), `fs_read` and `fs_write` (lists of paths under `$plugin_data`, `$workspace` or absolute paths), `secrets` (list of secret names), `llm` (table: `enabled`, `allow_model_override`, `allow_provider_override`, `allowed_models`), `tools_call` (list of tool names it may dispatch), `tools_override` (list of core tool names it may replace), `hooks` (list of hook names), `prompt_section` (max chars, at most 4000), `commands` (list of slash command names), `cli_commands` (list of `prometheus plugins run <id>` command names), `providers` (list of model-provider ids it may register), `providers_override` (list of built-in provider ids it may replace), `approval_transport` (bool), `inject_messages` (bool), `platform_actions` (list of verbs from `add_reaction`, `set_thread_title`), `subprocess` (bool) and `ui` (bool); the host SHALL deny any operation not covered by a granted capability with the error `capability_denied: <capability> <detail>` returned to the plugin and logged.
**Milestone:** M2

#### Scenario: Undeclared host is blocked
- **GIVEN** a plugin granted `network = ["api.weather.example"]`
- **WHEN** it issues an HTTP request to `exfil.example.net`
- **THEN** the host SHALL refuse the request with `capability_denied: network exfil.example.net` and no connection SHALL be opened

### Requirement: Capability grants are user-approved and digest-bound
Enabling a plugin SHALL show its declared capabilities in plain language and require the user to approve them; approved grants SHALL be stored in the single profile grant store `<profile_home>/grants.toml` (owned by `security-boundaries`) with subject `plugin:<id>@<digest>`; non-interactive enable SHALL require `--grant all` or `--grant <capability>,...` and otherwise exit 4 with `capability approval required`; an update whose manifest requests capabilities beyond the stored grant SHALL keep running the previously granted version until the user approves the new set; `prometheus plugins grants show|approve|revoke <id> [<capability>]` SHALL manage grants, and a revoked capability SHALL take effect on the next call.
**Milestone:** M2

#### Scenario: Capability escalation on update waits for approval
- **GIVEN** plugin `weather` 1.0 granted `network = ["api.weather.example"]`, and version 1.1 adds `secrets = ["GITHUB_TOKEN"]`
- **WHEN** `prometheus plugins update weather` runs non-interactively
- **THEN** the system SHALL keep 1.0 active and report `weather 1.1 requests new capabilities: secrets GITHUB_TOKEN`

### Requirement: WASM sandbox resource limits
The host SHALL run each WASM plugin instance with linear memory capped at `plugins.wasm.max_memory_bytes` (default 268435456), CPU bounded per call by `plugins.wasm.call_timeout_ms` (default 30000) via fuel and epoch interruption, no ambient filesystem, network, clock-precision or environment access beyond granted capabilities, and SHALL convert a trap or timeout into a call error naming the plugin; after 3 traps within 60 seconds the plugin SHALL be suspended for the rest of the session and reported in `prometheus plugins doctor`.
**Milestone:** M2

#### Scenario: Infinite loop is interrupted
- **GIVEN** a tool plugin whose function loops forever
- **WHEN** the model calls the tool
- **THEN** the call SHALL fail within 30 s plus scheduling slack with `plugin '<id>' exceeded call timeout` and the turn SHALL continue

### Requirement: Host interfaces offered to plugins
The host SHALL offer plugins only these imports, each gated by the matching capability: `log` (always), `state` (key-value and files under `<profile_home>/plugin-data/<id>/`), `http` (`network`), `secrets.get` (`secrets`, returning only granted names), `llm.complete` and `llm.complete-structured` (`llm`), `tools.dispatch` (`tools_call`), `events.emit` and `events.subscribe` (always, namespaced), `config.get` and `config.set` (scoped to `plugins.entries.<id>.*`) and `session.info` (read-only session id, profile name, surface, cwd); plugins SHALL NOT be able to read process environment variables.
**Milestone:** M2

#### Scenario: Secret outside the grant is not readable
- **GIVEN** a plugin granted `secrets = ["NOTION_API_KEY"]`
- **WHEN** it calls `secrets.get("OPENAI_API_KEY")`
- **THEN** the call SHALL return `capability_denied: secrets OPENAI_API_KEY`

### Requirement: Plugin state and profile scope
Plugin persistent data SHALL live under `<profile_home>/plugin-data/<id>/` resolved from the ProfileScope of the instance; the host SHALL create one plugin instance per profile, SHALL never share instance state between profiles, and SHALL bind hook and lifecycle calls made outside a turn (startup, timers, reloads) to the owning profile explicitly.
**Milestone:** M2

#### Scenario: Two profiles get two instances
- **GIVEN** plugin `notes` enabled in profiles `a` and `b` on one gateway
- **WHEN** a session in `a` writes state key `k`
- **THEN** reading `k` from `b`'s instance SHALL return nothing

### Requirement: Plugin tool registration
A plugin implementing the `tool` world SHALL register tools with JSON-Schema parameters; a tool name equal to a core tool SHALL be refused unless the name is in the granted `tools_override`, a name already registered by another plugin SHALL be refused with an error naming both plugins, and plugin tools SHALL join a session's tool list only at session start.
**Milestone:** M2

#### Scenario: Override needs a grant
- **GIVEN** a plugin registering `web_search` without `tools_override = ["web_search"]`
- **WHEN** it is loaded
- **THEN** the registration SHALL be refused and the core `web_search` SHALL remain

### Requirement: Typed hook registry
The system SHALL define a closed set of hook names: `pre_tool_call`, `post_tool_call`, `pre_llm_call`, `post_llm_call`, `pre_api_request`, `post_api_request`, `api_request_error`, `pre_auxiliary_call`, `post_auxiliary_call`, `on_session_start`, `on_session_end`, `on_session_finalize`, `on_session_reset`, `on_session_delete`, `on_turn_start`, `on_turn_end`, `post_compaction`, `pre_command`, `pre_gateway_dispatch`, `post_gateway_admission`, `post_gateway_delivery`, `gateway_platform_event`, `on_gateway_startup`, `on_gateway_shutdown`, `transform_terminal_output`, `transform_tool_result`, `transform_llm_output`, `transform_api_error_classification`, `on_stream_start`, `on_stream_delta`, `on_stream_end`, `on_interim_message`, `pre_verify`, `on_skill_lifecycle`, `subagent_start`, `subagent_stop`, `agent_loop_stopped`, `pre_approval_request`, `post_approval_response`, `on_human_input_request`, `on_human_input_resolved`, `on_room_member_activity`, `pre_transcription`, `kanban_task_claimed`, `kanban_task_completed`, `kanban_task_blocked`, `on_kanban_worker_spawned`, `on_kanban_worker_exited`, `on_kanban_worker_stale_claim`, `on_kanban_task_updated`, `on_kanban_dispatch_tick` and `on_plugin_loaded` (a superset of every Hermes plugin hook name, so a Hermes plugin's hook names all resolve); this list SHALL be the only hook registry (other capabilities, harness `hooks.toml` and the python bridge reference these names), and a plugin declaring or subscribing to an unknown hook name SHALL fail to load with `unknown hook '<name>'`.
**Milestone:** M2

#### Scenario: Typo is a load error
- **GIVEN** a plugin declaring `hooks = ["pre_tool_cal"]`
- **WHEN** it is loaded
- **THEN** the system SHALL refuse it with `unknown hook 'pre_tool_cal'`

### Requirement: Hook payloads are versioned and additive
Hook payloads SHALL be WIT records whose fields are added only as optional fields within a world's major version, each payload SHALL carry `schema_version` (`prometheus.hooks.v1`), and the host SHALL deliver to a plugin compiled against an older minor version only the fields that version declares.
**Milestone:** M2

#### Scenario: Old plugin receives a new payload
- **GIVEN** a plugin built against hook world 1.0 and a host on 1.3 that added field `cost_usd` to `post_llm_call`
- **WHEN** `post_llm_call` fires
- **THEN** the plugin SHALL receive the 1.0 fields and the call SHALL succeed

### Requirement: Hook isolation and time bounds
Hook callbacks SHALL run with errors caught and logged (one WARN per distinct plugin, hook and error, DEBUG for repeats) without stopping remaining callbacks; each callback SHALL be bounded by `plugins.hook_timeout_ms` (default 30000, valid range 1 to 600000, values outside fail config validation), a timed-out callback SHALL count as no result and be suspended for 60 seconds, and hooks SHALL run in a deterministic order (plugin load order, then registration order).
**Milestone:** M2

#### Scenario: Failing hook does not block others
- **GIVEN** two `post_tool_call` callbacks where the first raises
- **WHEN** a tool completes
- **THEN** the second callback SHALL still run and the tool result SHALL be unchanged

### Requirement: pre_tool_call directives
The system SHALL interpret `pre_tool_call` results as `modify` (shallow-merge `args` into the tool arguments, re-validated against the tool schema), `block` (message required; the tool is not run and the message becomes the tool result) or `approve` (escalate to the human approval gate with the message and optional `rule_key`), applying precedence block over approve over modify regardless of order, and an approval gate that errors, is denied or times out SHALL block; a `modify` producing schema-invalid arguments SHALL block with the validation error.
**Milestone:** M2

#### Scenario: Block beats approve
- **GIVEN** one plugin returning `approve` and another returning `block` for the same call
- **WHEN** the tool call is dispatched
- **THEN** the tool SHALL NOT run and no approval prompt SHALL be shown

### Requirement: pre_llm_call context preserves the prompt cache
The system SHALL append non-empty `pre_llm_call` context to the current turn's user message only (never the system prompt, never persisted in stored history), and SHALL spill context from one callback larger than `plugins.hook_context_max_chars` (default 10000) to `<profile_home>/hook_outputs/<session_id>/` replacing it with a 500-character head, a 500-character tail and the spill path.
**Milestone:** M2

#### Scenario: Large hook context is spilled
- **WHEN** a `pre_llm_call` callback returns 50,000 characters
- **THEN** the user message SHALL contain about 1,000 characters plus the spill path and the file SHALL hold the full text

### Requirement: Plugin system-prompt sections are bounded and frozen
A plugin with the `prompt_section` capability SHALL register sections with ids matching `^[a-z0-9][a-z0-9._-]{0,127}$`, at most the granted size and at most 4000 characters each, with the aggregate across plugins capped at 8000 characters (sections beyond the cap dropped in load order with a WARN), and duplicate ids refused; sections SHALL be rendered only when a session's prompt is built and SHALL NOT change within that session.
**Milestone:** M2

#### Scenario: Duplicate section id refused
- **GIVEN** plugin `a` registered section `style.guide`
- **WHEN** plugin `b` registers `style.guide`
- **THEN** the system SHALL refuse with `system prompt section 'style.guide' is already registered by plugin 'a'`

### Requirement: Plugin slash commands
A plugin SHALL register only the slash commands listed in its granted `commands` capability; a command name equal to a core command, alias or skill slug already registered SHALL be refused with an error naming the owner, and commands SHALL be available on every surface whose command registry the plugin targets.
**Milestone:** M2

#### Scenario: Core command cannot be taken
- **WHEN** a plugin registers `/model`
- **THEN** the registration SHALL be refused and `/model` SHALL keep its core behavior

### Requirement: Plugin event bus
`events.emit(name, payload)` SHALL publish as `<plugin_id>:<name>` and SHALL reject an empty name or one containing `:` (the `prometheus:` namespace is reserved for core events); delivery SHALL be asynchronous, ordered per emitter, and a subscriber failure SHALL NOT affect the emitter.
**Milestone:** M2

#### Scenario: Reserved namespace rejected
- **WHEN** a plugin emits `prometheus:session_start`
- **THEN** the call SHALL fail with an invalid-name error

### Requirement: Plugin-bundled skills
A plugin MAY ship skills under its `skills/` directory, which the system SHALL expose read-only as `<plugin_id>:<skill>` while the plugin is enabled, without copying them into the profile skills directory, subject to the same gating, scanning and prompt-cache rules as other skills.
**Milestone:** M2

#### Scenario: Disabling a plugin hides its skills next session
- **GIVEN** enabled plugin `kanban` shipping skill `board`
- **WHEN** the plugin is disabled and a new session starts
- **THEN** `kanban:board` SHALL NOT appear in the skills index

### Requirement: Plugin LLM access gate
`llm.complete` SHALL run against the profile's active provider and model by default and SHALL refuse a request specifying a provider or model unless `allow_provider_override` or `allow_model_override` is granted and the value is in `allowed_models` (`["*"]` meaning any); every plugin LLM call SHALL be charged to the session's cost ledger under the plugin id and SHALL count toward the session budget.
**Milestone:** M2

#### Scenario: Model override without grant
- **GIVEN** a plugin with `llm = { enabled = true }`
- **WHEN** it calls `llm.complete` with `model = "gpt-5"`
- **THEN** the call SHALL fail with `capability_denied: llm model override`

### Requirement: Provider-object plugins fail closed when unregistered
Plugins implementing the `provider` world SHALL register image-generation, video-generation, web-search, web-extract, text-to-speech, transcription and secret-source providers, selected by `image_gen.provider`, `video_gen.provider`, `web.search_backend`, `web.extract_backend`, `tts.provider`, `stt.provider` and `secrets.sources`; when one of these keys names a provider that is not registered, every category SHALL behave the same way: the feature SHALL be unavailable with the error `<category> provider '<name>' is not installed or not enabled`, never silently fall back to another provider.
**Milestone:** M2

#### Scenario: Missing image provider does not fall back
- **GIVEN** `image_gen.provider = "fal"` and no enabled plugin registering `fal`
- **WHEN** the model calls the image generation tool
- **THEN** the tool SHALL return `image_gen provider 'fal' is not installed or not enabled`

### Requirement: Load order, dependencies and deadline
The system SHALL load enabled plugins in `requires_plugins` dependency order with id-alphabetical tie-breaks, SHALL refuse a plugin whose required plugin is missing, disabled or failed with `missing dependency '<id>'`, SHALL refuse every plugin in a dependency cycle naming the cycle, and SHALL bound each plugin's instantiation and registration by `plugins.load_timeout_ms` (default 10000, range 1 to 600000), skipping an overrunning plugin with `load timed out` while others continue.
**Milestone:** M2

#### Scenario: Cycle refuses only its members
- **GIVEN** plugins `a` and `b` requiring each other and an unrelated plugin `c`
- **WHEN** plugins load
- **THEN** `a` and `b` SHALL be refused naming the cycle and `c` SHALL load

### Requirement: Hot reload respects the prompt-cache invariant
`prometheus plugins reload [<id>]`, `/plugins reload` and every install, enable, disable or update SHALL replace plugin instances after in-flight calls on the old instance finish (bounded by the call timeout), activating hooks, slash commands, event subscribers, platform adapters and UI panels immediately for all sessions, while tool registrations, system-prompt sections, memory providers and context engines SHALL take effect only in sessions started afterwards; `--now` SHALL additionally rebuild the current session's tools and prompt and record a `prompt.rebuilt` event with `reason` `plugins --now`; the command SHALL print which parts became active now and which are deferred.
**Milestone:** M2

#### Scenario: New tool waits for the next session
- **GIVEN** an active session and a plugin update that adds tool `forecast`
- **WHEN** the user runs `/plugins reload weather`
- **THEN** the current session's tool list SHALL NOT contain `forecast`, a new session's SHALL, and the output SHALL say `tools: next session`

### Requirement: Plugin development loop (upgrade)
`prometheus plugins dev <path> [--watch]` SHALL load a local plugin directory into the current profile as `dev:<id>` without installing or signing it, granting only the capabilities the developer approves at start, and with `--watch` SHALL rebuild (running the manifest's `build` command when present) and hot-reload it on every source change under the reload rules, printing load errors and capability denials inline; dev plugins SHALL never load in gateway or cron sessions.
**Milestone:** M3

#### Scenario: Edit-reload cycle
- **GIVEN** `prometheus plugins dev ./my-plugin --watch` running
- **WHEN** the developer changes a hook implementation and saves
- **THEN** the plugin SHALL be rebuilt and the new hook SHALL handle the next event without restarting Prometheus

### Requirement: Safe mode
`prometheus --safe-mode` and `plugins.safe_mode = true` SHALL skip instantiating every non-embedded plugin, MCP server and python-bridge sidecar for that process, SHALL show a banner stating safe mode is active, and SHALL leave plugin configuration unchanged; the `--safe-mode` flag SHALL additionally imply `--ignore-user-config` and `--ignore-rules` (no profile `config.toml` values, context files, `SOUL.md` or memory injection), SHALL skip shell hooks, gateway hook directories and outbound webhooks, SHALL require explicit `--model` and `--provider` (otherwise exit 78 with `--safe-mode reads no profile default model: launch with --model <m> --provider <p>`), SHALL start even when `config.toml` fails to parse, and when the turn is served by a running daemon SHALL run it in an isolated worker that never reads the profile's config.
**Milestone:** M2

#### Scenario: Broken config does not block safe mode
- **GIVEN** a profile whose `config.toml` has a syntax error
- **WHEN** the user runs `prometheus chat --safe-mode --model gpt-5 --provider openai -q "hi"`
- **THEN** the turn SHALL run with code defaults and no hooks, MCP servers or plugins

#### Scenario: Broken plugin bypassed
- **GIVEN** an enabled plugin that crashes at load
- **WHEN** the user runs `prometheus --safe-mode chat`
- **THEN** the session SHALL start without loading that plugin

### Requirement: Plugin catalog
The system SHALL resolve bare plugin names through a catalog of entries with `id`, `repo` (`https://` only), `ref` (full 40-hex commit SHA or package digest `sha256:<64 hex>`), `tier` (`official`, `verified` or `community`), `category`, `capabilities_summary` and `publisher_key`; the live catalog SHALL be fetched from `plugins.catalog_url`, cached under the profile's `cache/` directory for 6 hours, rejected when larger than 2 MiB or failing signature verification against the release-embedded catalog key, written atomically, and on any failure the catalog embedded in the binary SHALL be used; invalid entries SHALL be skipped with a WARN.
**Milestone:** M2

#### Scenario: Tampered catalog is ignored
- **GIVEN** a live catalog response whose signature does not verify
- **WHEN** the user runs `prometheus plugins search weather`
- **THEN** the system SHALL use the embedded catalog and log a WARN naming the signature failure

### Requirement: Catalog kill list
The system SHALL refuse to install, enable, update or load a plugin whose id or canonical repo (host/owner/repo, case-, scheme-, `www.`- and `.git`-insensitive) is on the union of the embedded and cached live `removed` list, with `Plugin '<id>' was removed from the catalog and is blocked: <reason>`, unless its install record carries `allow_removed = true`, which only `prometheus plugins install --allow-removed` sets.
**Milestone:** M2

#### Scenario: Removed plugin stops loading
- **GIVEN** an installed plugin later added to the removed list
- **WHEN** the next session starts
- **THEN** the plugin SHALL NOT load and `prometheus plugins list` SHALL show the removal reason

### Requirement: Signed plugin packages and provenance
Plugins SHALL be distributed as `.pmtpkg` archives (manifest, component, assets, skills) with a detached signature over the package digest; the system SHALL verify the signature against the catalog entry's `publisher_key` or a Sigstore identity, SHALL apply `plugins.signature_policy` (`require` default, `warn`, `off`) to local packages, where `require` refuses unsigned packages unless installed with `--allow-unsigned` (recorded on the install record), while catalog installs SHALL always require a valid signature unless `--allow-unsigned` is given and recorded, and an invalid signature is always refused, and SHALL write an install record `<profile_home>/plugins/<id>/.install.toml` (source, ref, package digest, signer, tier, installed_at, flags) which is the only authoritative provenance; `prometheus plugins pack` and `prometheus plugins sign` SHALL produce packages and signatures.
**Milestone:** M2

#### Scenario: Unsigned local package needs an explicit flag
- **GIVEN** `plugins.signature_policy = "require"` and an unsigned `weather.pmtpkg`
- **WHEN** the user runs `prometheus plugins install ./weather.pmtpkg`
- **THEN** the system SHALL exit 77 with `package is unsigned; re-run with --allow-unsigned to install anyway`

### Requirement: plugins CLI
The system SHALL provide `prometheus plugins install|search|browse|show|list|enable|disable|update|check-updates|remove|grants|capabilities|adopt|validate|pack|sign|doctor|reload|dev|test-adapter|run|collection|hooks`, and the slash command `/plugins [list|reload [<id>] [--now]]` on every interactive surface (bare `/plugins` lists id, version, kind, state and load error); `install <identifier>` SHALL accept a catalog id (resolved to its pinned ref), a `.pmtpkg` path, a Git URL with `--ref <full SHA>` (a short or symbolic ref SHALL be refused), and without `--enable`/`--no-enable` SHALL ask `Enable '<id>' now? [y/N]` on a TTY and leave it disabled otherwise; `validate <path>` SHALL check the manifest, WIT world compatibility and capability declarations and exit 65 on any error; `check-updates` and `update` with no id SHALL list (respectively update) every plugin whose catalog ref or source moved; `capabilities [<id>]` SHALL print declared versus granted capabilities per plugin; and `adopt <dir>` SHALL register a plugin directory placed by hand under `<profile_home>/plugins/` by validating it, writing an install record with source `adopted` and the computed digest, and running the normal capability-grant flow (an unadopted directory SHALL never load).
**Milestone:** M2

#### Scenario: Hand-placed plugin needs adoption
- **GIVEN** a directory `<profile_home>/plugins/notes/` copied by hand with no install record
- **WHEN** the daemon starts
- **THEN** `notes` SHALL NOT load and `prometheus plugins list` SHALL show it as `unadopted` until `prometheus plugins adopt` completes

#### Scenario: Symbolic ref refused
- **WHEN** the user runs `prometheus plugins install https://github.com/x/y --ref main`
- **THEN** the system SHALL exit 2 with `--ref must be a full commit SHA`

### Requirement: MCP client servers
The system SHALL connect to MCP servers declared in `[mcp.servers.<name>]` (`transport = "stdio"` with `command`, `args`, `env_secrets`, or `transport = "http"` with `url` and optional `auth_secret`), launching stdio servers as sandboxed child processes whose network and filesystem access follows the server's `capabilities` table exactly like a plugin grant; an unreachable or crashed server SHALL mark only its tools unavailable for new sessions with a reason in `prometheus mcp status`, restarts SHALL use exponential backoff from 1 s to 60 s, and `/mcp reload` SHALL follow the hot-reload rules (new tools next session unless `--now`).
**Milestone:** M2

#### Scenario: Crashed MCP server does not break the session
- **GIVEN** a session using tools from stdio MCP server `github` and the server process exits
- **WHEN** the model calls a `github` tool
- **THEN** the tool SHALL return `MCP server 'github' unavailable` and the session SHALL continue

### Requirement: MCP server mode
`prometheus mcp serve [--stdio | --http <addr>] [--profile <name>]` SHALL expose to MCP clients the toolsets listed in `mcp_server.expose_toolsets` (default `["skills"]`, i.e. read-only skill tools) as MCP tools, skills as MCP prompts and resources, and nothing else; `--http` SHALL bind loopback by default, SHALL require a bearer token from the secrets store (`MCP_SERVER_TOKEN`, generated on first use) and SHALL refuse a non-loopback address unless `mcp_server.allow_remote = true`; every exposed tool call SHALL run under the profile's approval policy and sandbox, and calls needing approval with no interactive approver SHALL be denied.
**Milestone:** M2

#### Scenario: Terminal not exposed by default
- **GIVEN** default `mcp_server` settings
- **WHEN** an MCP client lists tools
- **THEN** the list SHALL contain only skill tools and SHALL NOT contain `terminal`

### Requirement: WIT world versioning and compatibility
Each WIT world SHALL carry a semver version; the host SHALL instantiate a component built against the current or the immediately previous major version of each world it uses and SHALL refuse others with `world '<name>@<version>' is not supported (host supports <range>)`; within a major version additions SHALL be optional, and a deprecated import SHALL warn once per process and remain for at least two subsequent minor releases.
**Milestone:** M2

#### Scenario: Previous major still loads
- **GIVEN** a host at `tool@3.x` and a plugin built against `tool@2.4`
- **WHEN** it is loaded
- **THEN** the plugin SHALL load and `prometheus plugins show` SHALL note `tool@2 (previous major)`

### Requirement: Python plugins route through the bridge
A plugin of kind `python` (or a directory containing a Hermes `plugin.yaml` with a `register` entry point) SHALL never be imported into the Prometheus process and SHALL be hosted by the python bridge under the same enablement, capability-grant, hook, reload and catalog rules as WASM plugins.
**Milestone:** M2

#### Scenario: Hermes plugin is hosted out-of-process
- **GIVEN** an enabled Hermes plugin directory `<profile_home>/plugins/spotify/` with `plugin.yaml`
- **WHEN** a session starts
- **THEN** the plugin's code SHALL run in a python-bridge sidecar process and its tools SHALL be registered through the bridge

### Requirement: Transform hooks rewrite results and replies
The system SHALL treat `transform_tool_result` (payload: tool name, args, result, session id, tool call id, turn id, duration, status, error type) as a rewrite of one tool result before it is appended to the conversation, where the first returned string (including an empty string) replaces the result and no return leaves it unchanged; and `transform_llm_output` (payload: response text, session id, model, platform) as a rewrite of a turn's final reply before it is delivered and before the assistant message is persisted, where the first non-empty returned string wins, so the stored transcript always equals what the user saw. A plugin MAY register `transform_llm_output` with `hold_stream = true`, in which case for every session of that profile the reply text SHALL NOT be streamed (surfaces show only a busy or typing indicator), interim commentary SHALL be stored hidden, every way a turn ends SHALL pass through the hook, the hook set SHALL be frozen at turn start, and a hook that fails or times out without any rewrite SHALL withhold the reply and deliver `reply withheld by a plugin` instead of the original; `hold_stream` on any other hook SHALL be ignored with a WARN.
**Milestone:** M2

#### Scenario: Held reply never leaks
- **GIVEN** a redaction plugin registering `transform_llm_output` with `hold_stream = true` whose callback times out
- **WHEN** a Telegram turn finishes
- **THEN** the chat SHALL receive `reply withheld by a plugin` and no draft or streamed edit of the original text SHALL have been sent

#### Scenario: Tool result redaction
- **GIVEN** a `transform_tool_result` callback that replaces `sk-` keys with `[redacted]`
- **WHEN** `web_extract` returns text containing `sk-abc123...`
- **THEN** the model SHALL see `[redacted]` and the stored tool message SHALL equal what the model saw

### Requirement: Streaming and lifecycle observer hooks
The system SHALL fire `on_stream_start`, `on_stream_delta`, `on_stream_end` and `on_interim_message` off the token path with immutable normalized text payloads that cannot alter the stream; `on_skill_lifecycle` after a successful skill create, edit, install, enable, disable or delete (payload: skill name, action); `subagent_start` and `subagent_stop` around each delegated child (payload: parent session id, child session id, role, goal summary, and on stop outcome and duration); `agent_loop_stopped` when a running turn is interrupted (payload: session key, platform, reason); `post_gateway_admission` after an inbound message passes authorization; `on_room_member_activity` for hosted group-room member runtime events (never written to the durable room log); and `pre_transcription` after STT provider resolution, whose returned table MAY change only `prompt`, `language` and `model` (registration order, last writer wins). Return values of every hook in this requirement other than `pre_transcription` SHALL be ignored.
**Milestone:** M2

#### Scenario: Stream observer cannot change output
- **GIVEN** an `on_stream_delta` callback that returns a modified string
- **WHEN** a reply streams
- **THEN** the user SHALL receive the unmodified tokens and the return value SHALL be discarded

### Requirement: pre_verify continuation hook
The system SHALL fire `pre_verify` once per turn when the agent edited files in that turn and is about to finish (payload: session id, platform, model, `coding` bool, `attempt` count starting at 0, final response, sorted changed paths), and a result `{action = "continue", message}` (or the Claude-Code shape `{decision = "block", reason}`) SHALL append the message as a synthetic user message and continue the turn; consecutive continuations in one turn SHALL be capped by `agent.max_verify_nudges` (default 3), after which the turn finishes, and any other result SHALL let the turn finish.
**Milestone:** M2

#### Scenario: Endless continue is bounded
- **GIVEN** a `pre_verify` hook that always returns `continue`
- **WHEN** the agent finishes an editing turn
- **THEN** the loop SHALL continue at most 3 times and then deliver the final answer

### Requirement: transform_api_error_classification hook
The system SHALL fire `transform_api_error_classification` once per failed provider call before built-in classification (payload: provider, model, status code, error type, error code, error message, error body, approximate tokens, context length, message count), run every callback, and use the first valid result in registration order of the form `{reason, retryable?, should_compress?, should_rotate_credential?, should_fallback?, message?}` where `reason` is a known failover reason; invalid results or unknown reasons SHALL be skipped with a WARN; this hook SHALL NOT be registrable by shell hooks (refused at config validation).
**Milestone:** M2

#### Scenario: Provider plugin claims its quirk
- **GIVEN** a provider plugin returning `{reason = "model_not_found", should_fallback = true}` for its 404 body
- **WHEN** that provider returns the 404
- **THEN** the turn SHALL move to the next fallback route without retrying the same route

### Requirement: Approval and human-input observer hooks
The system SHALL fire `pre_approval_request` before any approval decision is requested on any surface, including smart-mode decisions without a human (payload: redacted command, description, pattern key(s), session key, surface such as `cli`, `gateway`, `smart`, `mcp-elicitation/<server>` or `mcp-trust/<server>`), `post_approval_response` after the decision (adding `choice` and `decided_by`), and `on_human_input_request` and `on_human_input_resolved` around every `clarify` or elicitation prompt; these hooks SHALL be observers whose return values are ignored, and a veto SHALL only be possible through `pre_tool_call`.
**Milestone:** M2

#### Scenario: Notifier observes approvals
- **GIVEN** a plugin that logs `pre_approval_request`
- **WHEN** a terminal command needs approval in a Slack session
- **THEN** the plugin SHALL receive surface `gateway` and the redacted command, and the approval flow SHALL be unchanged

### Requirement: Kanban lifecycle observer hooks
The system SHALL fire, after the kanban database commit and outside any board lock, `kanban_task_claimed` (dispatcher, before spawn), `kanban_task_completed` (adds `summary`) and `kanban_task_blocked` (adds `reason`) with payload task id, board, assignee, run id and profile name; `on_kanban_worker_spawned` (adds worker pid and workspace path), `on_kanban_worker_exited` (adds worker pid, `exit_kind` in `clean_exit`, `rate_limited`, `nonzero_exit`, `signaled`, `unknown`, exit code and retry status), `on_kanban_worker_stale_claim` (TTL-expired claim reclaimed), `on_kanban_task_updated` (changed field names only, never values) and `on_kanban_dispatch_tick` once per dispatcher tick after the dispatch lock is released (board, dry_run, outcome `ok`, `skipped_locked` or `idle`, counts); return values SHALL be ignored.
**Milestone:** M3

#### Scenario: Slow observer cannot hold the board
- **GIVEN** an `on_kanban_dispatch_tick` callback that sleeps 20 s
- **WHEN** the dispatcher ticks
- **THEN** the dispatch lock SHALL already be released and other board writers SHALL proceed

### Requirement: Shell hooks with first-use consent
The system SHALL run user-declared shell hooks from `[[hooks.<event>]]` tables in the profile `config.toml` (`event` from the closed hook registry, `command` required and split without a shell, `matcher` regex applied to the tool name for tool events only, `timeout_secs` default 60 and clamped to 300, `fail_closed` default false and honoured only for `pre_tool_call`) as sandboxed subprocesses under the session's execution sandbox, piping JSON `{hook_event_name, tool_name, tool_input, session_id, cwd, profile, extra}` on stdin and parsing stdout as JSON: `{action|decision = "block", message|reason}`, `{action = "modify", args}` or `{decision = "modify", tool_input}`, `{action = "approve", message?, rule_key?}`, `{context}` for `pre_llm_call` and `{action = "continue", message}` for `pre_verify`; exit code 2 from `pre_tool_call` SHALL block with stdout reason, else the first 400 stderr characters, else `Blocked by shell hook.`; other failures SHALL fail open with a WARN unless `fail_closed = true`, which blocks with `hook <command> failed closed: <reason>`. Each `(event, command, script sha256)` SHALL require consent on first use, stored in `grants.toml`; only a foreground interactive `prometheus chat` SHALL ask, while gateway, cron, ACP and MCP-serve contexts SHALL skip unapproved hooks with a WARN naming the profile, unless `--accept-hooks` or `hooks.auto_accept = true` is set; editing the script SHALL invalidate consent; plugin hooks SHALL run before shell hooks; and consent or config changes SHALL take effect within 30 seconds on every profile a gateway serves.
**Milestone:** M2

#### Scenario: Exit 2 blocks a tool call
- **GIVEN** an approved `pre_tool_call` hook with `matcher = "terminal"` whose script prints `policy violation` to stderr and exits 2
- **WHEN** the model calls `terminal`
- **THEN** the call SHALL NOT run and its result SHALL be `policy violation`

#### Scenario: Edited script needs re-approval
- **GIVEN** an approved hook script whose content changes on disk
- **WHEN** the gateway next registers hooks for that profile
- **THEN** the hook SHALL be skipped with a WARN until consent is given again

### Requirement: Hooks CLI
The system SHALL provide `prometheus plugins hooks list` (shell hooks with matcher, timeout, consent status, and outbound webhooks with `signed`/`UNSIGNED`), `prometheus plugins hooks test <event> [--for-tool <name>] [--payload-file <path>]` (fires matching shell hooks on a synthetic payload and prints each parsed directive exactly as the dispatcher would receive it), `prometheus plugins hooks allow <event> <command>`, `prometheus plugins hooks revoke <command>` and `prometheus plugins hooks doctor` (exec bit, consent, script digest drift, JSON validity, execution time; exit 1 on any failure).
**Milestone:** M2

#### Scenario: Test shows fail-closed outcome
- **GIVEN** a `fail_closed` `pre_tool_call` hook whose command does not exist
- **WHEN** the user runs `prometheus plugins hooks test pre_tool_call --for-tool terminal`
- **THEN** the output SHALL show a parsed `block` with `failed closed` and the command SHALL exit 0

### Requirement: Gateway hook directories
The gateway SHALL load event hooks from `<profile_home>/hooks/<name>/` directories containing `HOOK.toml` (`name`, `description`, non-empty `events`) plus either an executable `handler` (run with the shell-hook JSON protocol) or a Hermes `handler.py` (hosted by the python bridge; a Hermes `HOOK.yaml` is translated); `events` SHALL be names from the closed hook registry, and Hermes event names in a translated `HOOK.yaml` SHALL map onto it as `gateway:startup` → `on_gateway_startup` (`platforms`), `session:start` → `on_session_start`, `session:end` → `on_session_end`, `session:reset` → `on_session_reset`, `session:compress` → `post_compaction` (`old_session_id`, `in_place`, `compression_count`), `agent:start` → `on_turn_start`, `agent:step` → `post_llm_call` (`iteration`, `tool_names`), `agent:end` → `on_turn_end` (adds `response` truncated to 500 chars), `reaction:added`/`reaction:removed` → `gateway_platform_event` with `kind` `reaction_added`/`reaction_removed`, and `command:<name>` → `pre_command` with matcher `<name>`, a Hermes `<prefix>:*` wildcard expanding to every mapped name with that prefix; unlike Hermes, placement SHALL NOT be consent: each hook directory SHALL require approval of its handler digest in `grants.toml` before it runs, handlers SHALL run sandboxed and fire-and-forget (errors logged, never blocking the pipeline), and the CLI, TUI, desktop and cron SHALL NOT load them.
**Milestone:** M2

#### Scenario: Unapproved hook directory is inert
- **GIVEN** a new `<profile_home>/hooks/alert/` with a valid `HOOK.toml` and handler not yet approved
- **WHEN** the gateway starts
- **THEN** the handler SHALL NOT run and `prometheus plugins hooks list` SHALL show it as `pending approval`

### Requirement: Outbound webhooks
The system SHALL deliver hook events to `[[hooks.outbound]]` targets (`name`, `url`, `events` from the closed hook registry, optional `matcher` regex for tool events, `secret_name` naming a secrets-store entry, `timeout_secs` default 10 range 1 to 60) as HTTP POSTs with body `{hook_event_name, profile, tool_name, tool_input, session_id, cwd, extra, delivery_id, timestamp}` and headers `X-Prometheus-Event`, `X-Prometheus-Delivery` and, when a secret is set, `X-Prometheus-Signature-256: sha256=<HMAC-SHA256 hex of raw body>`; delivery SHALL be asynchronous through a bounded queue (default 1000 events, overflow dropped with a WARN), notify-only (responses ignored), retried once with backoff on connection errors and 5xx but not 4xx, never follow redirects, pass through the egress policy, refuse non-HTTPS URLs except loopback, and be skipped in safe mode; malformed entries SHALL fail config validation naming the entry.
**Milestone:** M2

#### Scenario: Signed delivery verifiable by receiver
- **GIVEN** an outbound target for `on_session_end` with `secret_name = "CI_HOOK_SECRET"`
- **WHEN** a session ends
- **THEN** the receiver SHALL get a POST whose `X-Prometheus-Signature-256` equals the HMAC of the body with that secret and whose `delivery_id` matches `X-Prometheus-Delivery`

#### Scenario: Dead endpoint never stalls a turn
- **GIVEN** an outbound target whose host never answers
- **WHEN** 50 tool calls complete
- **THEN** no tool call SHALL be delayed by delivery and failures SHALL be logged

### Requirement: Plugin CLI subcommands
A plugin granted `cli_commands` SHALL expose each listed command as `prometheus plugins run <plugin_id> <command> [args...]` (and `prometheus plugins run <plugin_id>` listing them), dispatched to the plugin with argv, the invoking ProfileScope, the caller's terminal size and TTY flag, streaming its stdout and stderr and exiting with the plugin's exit code (70 when it crashes); a memory-provider plugin's commands SHALL be listed only while it is the active `memory.provider`; `prometheus help` SHALL list installed plugin commands under a `plugin commands` heading; and a command name not in the grant SHALL be refused at registration.
**Milestone:** M2

#### Scenario: Honcho management command
- **GIVEN** the bridged `honcho` memory provider is active and registers CLI command `honcho`
- **WHEN** the user runs `prometheus plugins run honcho status`
- **THEN** the plugin's status output SHALL be printed and the command SHALL exit with the plugin's code

### Requirement: Approval transports
A plugin granted `approval_transport` SHALL be able to register a named transport that presents approval requests on an external channel, used only when `plugins.approval_transport = "<plugin_id>:<name>"` is set; the host SHALL pass an immutable request (redacted command and description, surface class, timeout, allowed choices, request id and digest), SHALL accept only a response bound to that id and digest with a choice the request offered, SHALL enforce `approvals.timeout_secs`, and SHALL treat errors, timeouts, stale or unbound responses and a missing registration as deny unless `plugins.approval_transport_fallback = "builtin"` (default `deny`) routes the prompt to the normal surface; hardline blocks SHALL be decided before any transport is called, and a transport SHALL NOT be able to auto-allow or set policy.
**Milestone:** M2

#### Scenario: Transport cannot widen scope
- **GIVEN** a once-only approval request
- **WHEN** the transport responds `always`
- **THEN** the host SHALL reject the response and deny the request

### Requirement: Plugin live messages and message injection
The host SHALL offer every plugin `conversation.post(text)` returning a handle and `conversation.edit(handle, text)`, bound to the session of the calling turn, hook or command (never a named chat), clamped to the platform message limit, edited in place where the surface supports edits and otherwise sent as a new message with `fallback = true`, and returning error codes `no_conversation`, `gateway_unavailable`, `adapter_unavailable`, `send_failed`, `edit_failed` or `timeout` (30 s) instead of failing; a plugin granted `inject_messages` SHALL be able to call `session.inject(content, role = "user", session_key | origin)` which queues text as new conversational input through the busy-input queue (never mutating history, never invoking slash commands, approvals or clarify answers), restricted to sessions of the plugin's own profile, re-checking current gateway authorization of the stored route, refusing an `origin` chat with no human message admitted yet, and returning false with a logged reason on any refusal.
**Milestone:** M2

#### Scenario: Progress message edits in place
- **GIVEN** a tool handler that posts `Migrating: 0/3` and edits it three times in a Discord session
- **WHEN** the tool runs
- **THEN** the channel SHALL show one message ending as the last edit

#### Scenario: Injection into another profile refused
- **GIVEN** a plugin in profile `a` granted `inject_messages`
- **WHEN** it injects with a session key belonging to profile `b`
- **THEN** the call SHALL return false and nothing SHALL be queued

### Requirement: Platform actions
A plugin granted `platform_actions` SHALL be able to call `platform.add_reaction(platform, chat_id, message_id, emoji)` and `platform.set_thread_title(platform, chat_id, thread_id, title)` for verbs in its grant, executed through connected adapters of the plugin's own profile whose capability descriptor declares the verb; every call SHALL return `{ok = true, action}` or `{ok = false, error, detail}` with error in `capability_not_granted`, `invalid_argument`, `gateway_unavailable`, `unknown_platform`, `adapter_not_registered`, `adapter_disconnected`, `unsupported_platform_action`, `action_failed`, SHALL never raise into hook dispatch, and SHALL be logged with plugin id, verb, platform and outcome; raw adapter handles SHALL never be exposed.
**Milestone:** M2

#### Scenario: Ungranted verb returns structured error
- **GIVEN** a plugin granted `platform_actions = ["add_reaction"]`
- **WHEN** it calls `set_thread_title`
- **THEN** the call SHALL return `{ok = false, error = "capability_not_granted"}` and no adapter call SHALL be made

### Requirement: Plugin collections
The system SHALL accept a shareable collection file (`prometheus-collection.toml`, and Hermes `hermes-pack.yaml` translated) listing plugins by catalog `name` or `repo` (+ optional `subdir`) each pinned to a full 40-hex commit SHA, optional non-secret `config` seeds for `plugins.entries.<id>`, and a `skills` list; `prometheus plugins collection show <file>` SHALL print a review of every plugin, source, ref and declared capabilities, `install <file>` SHALL show that review, ask one confirmation and then run the normal per-plugin install and capability-grant flow for each entry (no bulk grant, no `--yes`, refused non-interactively with exit 4), and `export [--enabled-only]` SHALL emit pinned entries for plugins with known provenance; a non-SHA ref SHALL be refused naming the entry, secret-shaped seed keys (`*token*`, `*key*`, `*secret*`, `*password*`) and grants SHALL be rejected on install and stripped on export, existing user values SHALL win over seeds, and per-plugin failures SHALL be reported with the rest continuing and a final exit 5.
**Milestone:** M3

#### Scenario: Tag ref refused
- **WHEN** a collection entry pins `ref = "v1.2.0"`
- **THEN** `collection install` SHALL refuse it with `entry '<name>': ref must be a full commit SHA`

### Requirement: Install-time threat scan
Every plugin install, update and collection install SHALL statically scan the plugin tree with the same threat-pattern engine as the skills guard (credential-store exfiltration, reverse shells, destructive commands, persistence, obfuscated execution, prompt injection in agent-read text) and classify it `safe` (install), `caution` (findings shown; `Install anyway? [y/N]` or `--force`) or `dangerous` (refused; `--force` does not override); documentation prose, comments, changelogs and test fixtures SHALL lower a finding's severity by one step but never remove it from the report, while bundled skills and agent-read files keep full severity; a dangerous verdict on update SHALL keep the previous version active and disable the update until reviewed; a scanner error SHALL fail closed; and `plugins.scan_on_install = false` SHALL disable scanning only with the choice recorded on the install record.
**Milestone:** M2

#### Scenario: Reverse shell blocked even with force
- **GIVEN** a plugin whose `setup.sh` opens a reverse shell
- **WHEN** the user runs `prometheus plugins install ./p.pmtpkg --force`
- **THEN** the install SHALL be refused with the verdict `dangerous` naming the finding, file and line, and exit 77

### Requirement: Python package and Nix plugin sources
The system SHALL install a Python-distributed Hermes plugin with `prometheus plugins install pypi:<dist>==<exact version>` by resolving the distribution's `hermes_agent.plugins` (or `prometheus.plugins`) entry point into a kind `python` plugin with its own pinned bridge environment and an install record of source `pypi`; and the Nix flake's module SHALL declare plugins (directory sources and Python packages) that are materialized as `<profile_home>/plugins/nix-managed-<id>/` with install record source `nix`, enabled through the declared config, and removed when dropped from the Nix configuration, never touching user-installed plugins.
**Milestone:** M3

#### Scenario: Unpinned PyPI plugin refused
- **WHEN** the user runs `prometheus plugins install pypi:hermes-spotify`
- **THEN** the system SHALL exit 2 with `pypi plugins need an exact version: pypi:<dist>==<version>`

### Requirement: Model-provider plugins
A plugin granted `providers` SHALL register model-provider descriptors (the `model-providers` descriptor contract) and optional request hooks (header building, request preparation, model listing) through the `provider` world or the python bridge; registered providers SHALL join the profile's provider registry at session boundaries, an id equal to a built-in provider SHALL be refused unless listed in the granted `providers_override`, an unregistered provider named by config SHALL fail with exit 78 as for built-ins, and a provider plugin's credentials SHALL be bound to the descriptor's `key_hosts` like any built-in.
**Milestone:** M2

#### Scenario: Plugin cannot silently replace a built-in
- **GIVEN** a plugin registering provider id `openrouter` without `providers_override`
- **WHEN** it loads
- **THEN** the registration SHALL be refused naming the built-in and the built-in `openrouter` SHALL stay active

### Requirement: Catalog tool for agent-initiated installs
The system SHALL provide a `catalog_manage` tool in a `catalog` toolset available only on surfaces that can draw an approval card (desktop, TUI), with `action` `search` (optional `kind` `plugin` or `skill`, `query`; at most 10 results with id, kind, display name, tier, installed state) or `install` (`items` list of `{kind, id}` from search, optional one-line `reason`); `install` SHALL show one card with a row per item and block until each row is installed, skipped or failed, the host deciding source, pinned ref, scan, grants and profile; unknown parameters SHALL be rejected naming them, and on other surfaces the tool SHALL return `catalog_manage needs the desktop app or TUI; install with prometheus plugins install <id> or prometheus skills install <id>`.
**Milestone:** M3

#### Scenario: Model cannot pick a version
- **WHEN** the model calls `catalog_manage` with `items = [{kind = "plugin", id = "weather", ref = "abc"}]`
- **THEN** the tool SHALL fail with `unknown item field(s) ref` and no card SHALL be shown

### Requirement: First-party disk-cleanup plugin
The system SHALL embed a disabled-by-default `disk-cleanup` plugin that, on `post_tool_call` of `write_file`, `patch` or `terminal`, tracks files matching `test_*`, `tmp_*` or `*.test.*` created under the profile home or `<tmp>/prometheus-*` as `test`, `temp` or `cron-output`, and on `on_session_end` deletes tracked `test` files, `temp` older than 7 days, `cron-output` older than 14 days and empty non-protected directories, while `research` (older than 30 days beyond the 10 newest), browser profiles (older than 14 days) and files over 500 MB are only listed for confirmation by `deep`; it SHALL never touch paths outside those roots, Windows mounts, protected top-level state directories (`logs`, `skills`, `plugins`, `cache`, `trajectories`, `snapshots`, `kanban`) or user workspace trees, SHALL keep `tracked.json` and an append-only `cleanup.log` under its plugin data, and SHALL provide `/disk-cleanup status|dry-run|quick|deep|track <path> <category>|forget <path>`.
**Milestone:** M3

#### Scenario: Project test file is never swept
- **GIVEN** the plugin enabled and the agent writes `test_api.py` inside the user's repository
- **WHEN** the session ends
- **THEN** the file SHALL NOT be tracked or deleted

### Requirement: First-party google_meet plugin
The system SHALL embed a disabled-by-default `google_meet` plugin registering toolset `google_meet` with `meet_join` (`url` restricted to `https://meet.google.com/...`, `mode` `transcribe` default or `realtime`, `guest_name`, `duration`, `headed`, `node`), `meet_status` (alive, joined, lobby, caption line count, last caption time, `micState`), `meet_transcript` (`last` N lines), `meet_say` (realtime mode only) and `meet_leave`, available only when its browser runtime and saved Google sign-in state are present; transcripts SHALL come from Meet's live captions (no meeting audio decoded), realtime replies SHALL be synthesized and played through a virtual microphone, artifacts SHALL be written under `<profile_home>/workspace/meetings/<meeting_id>/`, the join SHALL run as a background child and return immediately, and the plugin SHALL provide `prometheus plugins run google_meet setup|install|auth|join|status|transcript|say|stop|node`.
**Milestone:** M3

#### Scenario: Non-Meet URL rejected
- **WHEN** the model calls `meet_join` with `https://zoom.us/j/123`
- **THEN** the tool SHALL fail with `only meet.google.com URLs are accepted` and no browser SHALL start

### Requirement: Catalog plugins for Home Assistant and Camofox
The official catalog SHALL include `homeassistant` (specified once by `platform-adapters` › Home Assistant adapter and toolset) and `camofox` (a `tool` plugin exposing browser tools against a Camofox anti-detection Firefox REST server at `plugins.entries.camofox.url`); `hermes-migration` SHALL install either plugin (disabled, without grants) for a profile whose Hermes config used it (`HASS_TOKEN` or `platforms.homeassistant`; `CAMOFOX_URL` or `browser.cloud_provider = camofox`, which also disables the core `browser` toolset in that profile).
**Milestone:** M3

#### Scenario: Migrated Camofox profile
- **GIVEN** a Hermes profile with `browser.cloud_provider = camofox` and `CAMOFOX_URL` set
- **WHEN** it is migrated
- **THEN** the `camofox` plugin SHALL be installed with `plugins.entries.camofox.url` set and the core `browser` toolset SHALL be disabled in that profile

### Requirement: MCP OAuth for HTTP servers
An HTTP MCP server with `auth = "oauth"` SHALL authenticate with OAuth 2.1 + PKCE: discovering the authorization server, identifying with a Client ID Metadata Document where supported and otherwise Dynamic Client Registration, or a pre-registered `oauth.client_id` / `oauth.client_secret_name`, honoring `oauth.client_name`, `oauth.scopes`, `oauth.redirect_host` (`127.0.0.1` default or `localhost`), `oauth.redirect_port`, `oauth.redirect_uri`, `oauth.flow` (`browser` default or `device`) and `oauth.timeout_secs` (default 300); it SHALL verify the metadata issuer (RFC 8414) and the redirect `iss` (RFC 9207), bind refresh tokens to the issuer that granted them (dropping them if the issuer changes), store tokens only in the secrets store, refresh under the cross-process refresh lock, and when refresh fails in a background context (gateway, reload, probe) SHALL park the server without opening a browser and report `needs re-authorization: prometheus mcp login <server>`; interactive login SHALL print the authorize URL, accept a pasted redirect URL, and never open a browser on its own from a non-foreground process.
**Milestone:** M2

#### Scenario: Dead refresh token parks the server
- **GIVEN** a gateway with OAuth server `linear` whose refresh token is revoked
- **WHEN** the token expires
- **THEN** `linear` SHALL be parked, its tools unavailable for new sessions, no browser SHALL open, and `prometheus mcp status` SHALL show the re-auth command

#### Scenario: DCR-less server without client
- **GIVEN** an OAuth server that rejects dynamic registration and no `oauth.client_id`
- **WHEN** the user runs `prometheus mcp login drive`
- **THEN** the command SHALL exit 78 explaining that the server needs a pre-registered OAuth client

### Requirement: MCP mTLS and identity header
An HTTP MCP server SHALL accept `client_cert` (combined PEM path) with optional `client_key` path and `client_key_password_secret`, presenting the certificate in the TLS handshake and failing with `MCP server '<name>': client certificate not found: <path>` when a file is missing; and `identity_header = { name, value_from = "static" | "profile", value? }` sending the literal `value` or the active profile name on every request, never overriding a same-named entry in `headers` (case-insensitive), with an invalid block or one set on a stdio server warned and ignored.
**Milestone:** M2

#### Scenario: Profile identity per profile
- **GIVEN** profiles `alice` and `bob` both using server `team_api` with `identity_header = { name = "X-User-Id", value_from = "profile" }`
- **WHEN** each calls a tool
- **THEN** the server SHALL receive `X-User-Id: alice` and `X-User-Id: bob` respectively

### Requirement: MCP config watch, recycling and discovery wait
The daemon SHALL watch each served profile's `mcp.servers` and, when `mcp.auto_reload_on_config_change` is true (default), within 60 seconds disconnect removed or disabled servers and connect added ones, retrying failed first connects on a cooldown starting at 30 s and doubling to 10 minutes, with all tool-surface changes applying only to new sessions; a stdio server with `idle_timeout_secs` or `max_lifetime_secs` (default 0, never) SHALL be stopped after that idle time or age and restarted transparently on its next tool call while its tools stay registered; a server with `lazy = true` SHALL register tools from the schema cache and start on first call; and session start SHALL wait for still-connecting servers at most `mcp.discovery_wait_ms` (default 1500) interactively and `mcp.single_query_discovery_wait_ms` (default 15000) for one-shot `-q` runs, a server missing the window joining only later sessions.
**Milestone:** M2

#### Scenario: Recycled server restarts on demand
- **GIVEN** stdio server `playwright` with `idle_timeout_secs = 900`
- **WHEN** 15 minutes pass without a call and then the model calls one of its tools
- **THEN** the process SHALL have been stopped and SHALL be restarted transparently for that call with the session's tool list unchanged

### Requirement: mcp CLI
The system SHALL provide `prometheus mcp` (interactive picker on a TTY), `mcp add <name> (--url <url> | --command <cmd> [--args ...]) [--auth oauth|header] [--preset codex]` (probing tools and offering the tool checklist), `mcp remove <name>`, `mcp list`, `mcp test <name>` (connect, handshake, tool count, latency; exit 69 on failure), `mcp tools <name>` (re-open the tool selection, writing `tools.include` only when not all tools are selected), `mcp login <name> [--flow device]`, `mcp reauth <name>`, `mcp catalog`, `mcp install <entry>`, `mcp status` and `mcp serve`; the `codex` preset SHALL supply `command = "codex"`, `args = ["mcp-server"]` with explicit flags winning.
**Milestone:** M2

#### Scenario: Preset fills transport
- **WHEN** the user runs `prometheus mcp add my-codex --preset codex`
- **THEN** `mcp.servers.my-codex` SHALL contain `transport = "stdio"`, `command = "codex"` and `args = ["mcp-server"]`

### Requirement: MCP catalog entries
The plugin catalog SHALL also carry signed MCP entries (kind `mcp`) with `manifest_version`, transport, auth type, URL or command and args, optional git source pinned to a full SHA with sandboxed `bootstrap` commands, setup fields (secret fields masked and stored in the secrets store), `tools.default_enabled` or `tools.default_excluded`, `post_install` notes and advisory `suggest` metadata (`keywords`, `hosts`, `applications` up to 16, `examples` up to 6, `requires_app`); entries SHALL install disabled until the user enables them, SHALL pre-check tools from the user's prior selection, else `default_enabled`, else all, SHALL install even when the probe fails (applying defaults), SHALL be shown with a `requires a newer Prometheus` warning rather than hidden when their manifest version is newer, SHALL never auto-update, and SHALL substitute `${VAR}` placeholders at connect time only from the secrets store and the context variables `${userHome}`, `${workspaceFolder}`, `${workspaceFolderBasename}` and `${pathSeparator}`; agent-initiated installs SHALL go through the `connectors` setup card.
**Milestone:** M2

#### Scenario: Large surface uses an exclude list
- **GIVEN** a catalog entry declaring `tools.default_excluded = ["*_delete*"]`
- **WHEN** it is installed
- **THEN** no checklist SHALL be shown and `tools.exclude` SHALL be written with that list

### Requirement: MCP sampling
The system SHALL answer MCP `sampling/createMessage` requests per server under `[mcp.servers.<name>.sampling]` with `enabled` (default true for `trust = "full"` servers and false for `untrusted` servers), `model` override (which must be in `allowed_models` when that list is non-empty), `max_tokens_cap` (default 4096), `timeout_secs` (default 30), `max_rpm` (default 10, sliding window), `max_tool_rounds` (default 5), charging every sampling call to the session cost ledger under `mcp:<server>` and counting toward the session budget, and returning a protocol error naming the exceeded limit otherwise.
**Milestone:** M2

#### Scenario: Rate limit enforced
- **GIVEN** a server with `max_rpm = 10`
- **WHEN** it sends 11 sampling requests within one minute
- **THEN** the 11th SHALL be refused with a rate-limit error and no model call SHALL be made

### Requirement: MCP elicitation
The system SHALL route form-mode MCP `elicitation/create` requests (enabled per server by `elicitation.enabled`, default true) through the clarify and approval surface of the owning session, offering only answer-once or decline, unaffected by YOLO or `approvals.mode = "off"`, declining after `elicitation.timeout_secs` (default 300) or immediately when no interactive surface exists, and SHALL decline URL-mode elicitations as unsupported.
**Milestone:** M2

#### Scenario: Unattended elicitation declined
- **GIVEN** a cron job calling an MCP tool whose server requests elicitation
- **WHEN** the request arrives
- **THEN** it SHALL be declined immediately and the tool call SHALL continue with the server's decline handling

### Requirement: MCP tool costs and spend limits
An MCP server entry MAY declare `tool_costs` (tool name to a number or `{credits = [min, max], note}`), `spend_limit` (per conversation, 0 meaning none) and `cost_unit` (default `credits`); the approval prompt for a costed tool SHALL state the cost and the conversation's running total, the total SHALL follow the conversation across compression and branches, a costed call that would cross `spend_limit` SHALL ask for once-only approval even on a `trust = "full"` server or with YOLO, and where no one can answer SHALL be refused with a result telling the model not to retry; a malformed key SHALL skip the server with an error naming it, and a `tool_costs` name the server does not offer SHALL be warned at connect.
**Milestone:** M2

#### Scenario: Budget crossing asks even when trusted
- **GIVEN** server `renders` with `trust = "full"`, `tool_costs.create_image = 20`, `spend_limit = 100` and 90 credits used in this chat
- **WHEN** the model calls `create_image`
- **THEN** the user SHALL be asked once-only approval mentioning `about 20 credits` and `90 credits ... (limit 100)`

### Requirement: MCP events
An MCP server with `events = true` SHALL get utility tools `list_event_types`, `subscribe_event` (event arguments, `instructions` at most 2000 characters, `deliver` target) and `unsubscribe_event`; every subscription SHALL require approval showing the instructions and destination, SHALL be stored as a dynamic webhook route with its own signing secret and replay cursor, delivered via the webhook adapter's public HTTPS URL (plain HTTP refused), deduplicated by `eventId`, refreshed before the server-granted lifetime expires while connected, removed when the server refuses refresh permanently or ends it (notifying the `deliver` target), with each event starting an agent run under the webhook route's constrained toolset and its payload truncated to 32000 characters with a note.
**Milestone:** M3

#### Scenario: Duplicate event ignored
- **GIVEN** an active subscription
- **WHEN** the server delivers the same `eventId` twice
- **THEN** only one agent run SHALL start

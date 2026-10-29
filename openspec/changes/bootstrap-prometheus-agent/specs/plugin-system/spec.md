# Plugin System

## Purpose
The plugin system is how Prometheus grows capability at the edges instead of in the core. Native plugins are WebAssembly components that implement typed WIT worlds (`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`, `context-engine`, `terminal-backend`, `ui-panel`) and run in the host with only the capabilities the user granted; external tools arrive through MCP servers, and Hermes-style Python plugins run out-of-process through the python bridge. The key decisions are that no third-party code runs in-process with ambient authority, that every registry (hooks, commands, worlds, capabilities) is typed so unknown names fail at load, and that hot reload never mutates a live session's cached prompt or tool list.

## ADDED Requirements

### Requirement: Plugin kinds and manifest
Every plugin SHALL be a directory containing `plugin.toml` with `manifest_version` (integer, currently 1), `id` (matching `^[a-z0-9][a-z0-9_-]{0,63}$`), `version` (semver), `kind` (`wasm`, `mcp` or `python`), `description`, `license`, `requires_prometheus` (semver range), `worlds` (list of WIT world names with versions, for `wasm`), `capabilities` (table) and optional `requires_plugins`; for `python` kind a Hermes `plugin.yaml` SHALL be accepted in place of `plugin.toml` and translated by the python bridge. A manifest with an unknown kind, world, capability key or top-level field, or with a `manifest_version` greater than supported, SHALL be refused with an error naming the field, and a `requires_prometheus` range excluding the running version SHALL be refused before instantiation.
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
A plugin SHALL declare every authority it needs in `[capabilities]` using only these keys: `network` (list of `host[:port]` patterns, `*.` prefix allowed, no bare `*`), `fs_read` and `fs_write` (lists of paths under `$plugin_data`, `$workspace` or absolute paths), `secrets` (list of secret names), `llm` (table: `enabled`, `allow_model_override`, `allow_provider_override`, `allowed_models`), `tools_call` (list of tool names it may dispatch), `tools_override` (list of core tool names it may replace), `hooks` (list of hook names), `prompt_section` (max chars, at most 4000), `commands` (list of slash command names), `subprocess` (bool) and `ui` (bool); the host SHALL deny any operation not covered by a granted capability with the error `capability_denied: <capability> <detail>` returned to the plugin and logged.
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
The host SHALL run each WASM plugin instance with linear memory capped at `plugins.wasm.max_memory_mib` (default 256), CPU bounded per call by `plugins.wasm.call_timeout_ms` (default 30000) via fuel and epoch interruption, no ambient filesystem, network, clock-precision or environment access beyond granted capabilities, and SHALL convert a trap or timeout into a call error naming the plugin; after 3 traps within 60 seconds the plugin SHALL be suspended for the rest of the session and reported in `prometheus plugins doctor`.
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
The system SHALL define a closed set of hook names: `pre_tool_call`, `post_tool_call`, `pre_llm_call`, `post_llm_call`, `pre_api_request`, `post_api_request`, `api_request_error`, `pre_auxiliary_call`, `post_auxiliary_call`, `on_session_start`, `on_session_end`, `on_session_finalize`, `on_session_reset`, `on_session_delete`, `on_turn_start`, `on_turn_end`, `post_compaction`, `pre_command`, `pre_gateway_dispatch`, `post_gateway_delivery`, `gateway_platform_event`, `on_gateway_startup`, `on_gateway_shutdown`, `transform_terminal_output` and `on_plugin_loaded`; this list SHALL be the only hook registry (other capabilities, harness `hooks.toml` and the python bridge reference these names), and a plugin declaring or subscribing to an unknown hook name SHALL fail to load with `unknown hook '<name>'`.
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
`prometheus --safe-mode` and `plugins.safe_mode = true` SHALL skip instantiating every non-embedded plugin, MCP server and python-bridge sidecar for that process, SHALL show a banner stating safe mode is active, and SHALL leave plugin configuration unchanged.
**Milestone:** M2

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
The system SHALL provide `prometheus plugins install|search|browse|show|list|enable|disable|update|remove|grants|validate|pack|sign|doctor|reload|dev|test-adapter`; `install <identifier>` SHALL accept a catalog id (resolved to its pinned ref), a `.pmtpkg` path, a Git URL with `--ref <full SHA>` (a short or symbolic ref SHALL be refused), and without `--enable`/`--no-enable` SHALL ask `Enable '<id>' now? [y/N]` on a TTY and leave it disabled otherwise; `validate <path>` SHALL check the manifest, WIT world compatibility and capability declarations and exit 65 on any error.
**Milestone:** M2

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

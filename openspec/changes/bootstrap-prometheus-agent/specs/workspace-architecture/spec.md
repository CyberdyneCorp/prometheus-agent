# Workspace Architecture

## Purpose
Defines the shape of Prometheus as a product: one `prometheus` binary (alias `pmt`) that provides every surface as a subcommand, the supported targets, the single-daemon process model, the versioned local JSON-RPC contract between the daemon and its clients, the error taxonomy and exit codes, startup/memory budgets, the repository health gates and `prometheus doctor`. The key decision is that every cross-process contract is generated from typed definitions and checked in, so clients and daemon cannot drift silently the way Hermes' TUI/desktop wire did.

## ADDED Requirements

### Requirement: Single binary with subcommand surfaces
The system SHALL ship every surface as a subcommand of one executable named `prometheus`, and an executable invoked as `pmt` (hardlink, symlink or copy of the same build) SHALL behave identically. The top-level subcommands SHALL be exactly (with their milestone in `cli` › Top-level command tree): `chat` (the default when no subcommand is given), `setup`, `config`, `secrets`, `grants`, `profile`, `sessions`, `model`, `fallback`, `auth`, `tools`, `toolsets`, `approvals`, `skills`, `memory`, `runtimes`, `usage`, `logs`, `doctor`, `prompt`, `trajectory`, `demo`, `backup`, `snapshots`, `update`, `debug`, `version`, `help`, `daemon` (process lifecycle: `run`, `start`, `stop`, `restart`, `status`, `ensure`, `install-service`, `uninstall-service`, `ticket`, `pair-device`, `revoke-device`), `gateway` (messaging adapters and outbox: `status`, `setup`, `platforms`, `list`, `pause`, `resume`, `reload`, `restart`, `stop`, `outbox`, `send`, `peer`, `proxy`, `enroll`, `selftest`, `slack-manifest`, `whatsapp pair`, `whatsapp-cloud setup`, `photon setup|status`, `telegram logout`, `feishu comment-rules`), `tui`, `console`, `webhooks`, `pairing`, `cron`, `pause`, `curator`, `plugins`, `mcp`, `bridge`, `providers`, `portal`, `local`, `moa`, `connectors`, `browser`, `vault`, `security`, `status`, `lsp`, `verify`, `worktrees`, `projects`, `journey`, `themes`, `cache`, `files`, `schema`, `rpc`, `replay`, `eval`, `migrate`, `bundle`, `uninstall`, `completions`, `identity`, `dashboard` (`register`, `pair`, `devices`), `serve`, `webapp`, `desktop` (alias `gui`), `acp`, `computer-use`, `egress`, `codex-runtime`, `kanban`, `evolve`, `harness`, `experience`, `pets`, `achievements`, plus the hidden `self-test` used by the updater and `__complete` used by shell completions; each capability that introduces one of these owns its arguments and nested subcommands, `cli` › Top-level command tree SHALL list the same set, and `prometheus help` SHALL additionally list the commands of enabled plugins under `plugins run <plugin_id>`. Running `prometheus` with no subcommand SHALL be equivalent to `prometheus chat`.
**Milestone:** M1

#### Scenario: Alias invocation
- **GIVEN** `pmt` is a symlink to the installed `prometheus`
- **WHEN** the user runs `pmt version --json`
- **THEN** the system SHALL print the same JSON object as `prometheus version --json`

#### Scenario: Unknown subcommand
- **WHEN** the user runs `prometheus chta`
- **THEN** the system SHALL print `unknown command 'chta' (did you mean 'chat'?)` to stderr and exit 2

### Requirement: Version and build identity
`prometheus version` SHALL print the semantic version, and `prometheus version --json` SHALL print an object with exactly the keys `version`, `build_id`, `commit`, `build_date`, `target`, `tier`, `channel`, `rpc_protocol`, `config_schema_version`, `store_schema_version`, `trajectory_grammar`. `build_id` SHALL be a hash that differs for any two distinct binaries and SHALL be the value the daemon uses for stale-build detection. `prometheus version --verbose` SHALL additionally list each optional surface (`tui`, `gateway`, `dashboard`, `acp`, `mcp`, `evolve`, `eval`) with `yes`/`no` for whether it is compiled in, and invoking a surface that is not compiled in SHALL exit 69 with `surface '<name>' is not included in this build`.
**Milestone:** M1

#### Scenario: Machine-readable version
- **WHEN** the user runs `prometheus version --json`
- **THEN** the output SHALL be a single JSON object whose `rpc_protocol` has the form `<major>.<minor>`

#### Scenario: Minimal build without dashboard
- **GIVEN** a build produced without the dashboard surface
- **WHEN** the user runs `prometheus dashboard`
- **THEN** the system SHALL print `surface 'dashboard' is not included in this build` and exit 69

### Requirement: Supported targets and support tiers
The system SHALL publish builds for these targets with these tiers: Tier 1 (full test suite on every merge) — `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`; Tier 2 (build plus offline smoke suite on every merge) — `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`, `aarch64-linux-android` (Termux). A capability that is weaker on a target (for example OS sandbox enforcement) SHALL be reported by `prometheus doctor` rather than silently assumed.
**Milestone:** M1

#### Scenario: Tier reported at runtime
- **GIVEN** a build for `aarch64-linux-android`
- **WHEN** the user runs `prometheus version --json`
- **THEN** `target` SHALL be `aarch64-linux-android` and `tier` SHALL be `2`

### Requirement: No mandatory external runtime
The core binary SHALL run CLI chat, the turn loop, built-in tools, storage, the gateway daemon and the TUI without any Python, Node.js or browser installation. Features that need a managed runtime (browser automation, the Python bridge) SHALL install it lazily on first use and SHALL fail with an error of kind `unavailable` naming the runtime and the install command when it is absent and installation is refused or impossible.
**Milestone:** M1

#### Scenario: Fresh machine without Python
- **GIVEN** a host with no Python interpreter on `PATH`
- **WHEN** the user runs `prometheus chat -z "hello" --provider mock`
- **THEN** the system SHALL answer and exit 0

### Requirement: Launch environment is read once
The system SHALL read the process environment exactly once at startup into an immutable launch configuration, SHALL accept only `PROMETHEUS_HOME`, `PMT_LOG`, `RUST_LOG`, `PMT_SECRET_*` and registered provider-native secret names from it, and SHALL pass an explicit ProfileScope (home path, config snapshot, secrets handle) to every operation that reads or writes profile data. Any other `PMT_*` or `PROMETHEUS_*` variable SHALL be ignored and reported as a warning by `prometheus doctor`.
**Milestone:** M1

#### Scenario: Non-secret env var has no effect
- **GIVEN** `PMT_MODEL=gpt-x` is exported and `config.toml` sets `model.default = "claude-y"`
- **WHEN** the user starts a chat
- **THEN** the session SHALL use `claude-y` and `prometheus doctor` SHALL warn `ignored environment variable PMT_MODEL (configuration lives in config.toml)`

### Requirement: Typed registries reject unknown names
Every name-keyed registry (subcommands, slash commands, RPC methods, RPC events, server-to-client requests, hook points, toolsets, tools, config keys, providers, adapters, plugin capabilities) SHALL be a closed, typed registry, and a lookup or a configuration value naming an unregistered entry SHALL fail at load time with an error of kind `invalid_input` that names the unknown entry and up to 3 closest registered names.
**Milestone:** M1

#### Scenario: Misspelled hook in a plugin manifest
- **GIVEN** a plugin manifest subscribing to hook `pre_tool_cal`
- **WHEN** the plugin is loaded
- **THEN** loading SHALL fail with `unknown hook 'pre_tool_cal' (did you mean 'pre_tool_call'?)` and the plugin SHALL NOT be enabled

### Requirement: One daemon per Prometheus home
The system SHALL allow at most one gateway daemon per Prometheus home, enforced by an exclusive OS file lock on `<PROMETHEUS_HOME>/run/gateway.lock`, and SHALL read daemon-wide settings (`daemon.rpc.*`, `gateway.http.*`, `gateway.relay.*`, `gateway.on_all_adapters_down`, `daemon.autostart`, `daemon.restart_drain_secs`, `daemon.watchdog.*`, `dashboard.*` listener settings, and the machine-wide `update.*`, `runtimes.*` and `process.*` sections) only from `<PROMETHEUS_HOME>/daemon.toml`, never from a profile's `config.toml`; a second `prometheus daemon start` for the same home SHALL exit 75 with `gateway already running (pid <pid>); attach with 'prometheus tui' or stop it with 'prometheus daemon stop'`. The daemon SHALL serve every profile of that home, each through its own ProfileScope.
**Milestone:** M2

#### Scenario: Second daemon refused
- **GIVEN** a running daemon for `~/.prometheus`
- **WHEN** the user runs `prometheus daemon start` again
- **THEN** the second process SHALL exit 75 without opening any `state.db` as a writer

### Requirement: Single-writer storage invariant
The system SHALL guarantee that each profile's `state.db` has exactly one writing process: when a daemon owns the home, `prometheus chat` and every other client SHALL attach to it as RPC clients; when no daemon runs, `prometheus chat` SHALL run the agent in-process and take the same home lock for its lifetime, and a daemon started meanwhile SHALL exit 75.
**Milestone:** M1

#### Scenario: Chat attaches to a running daemon
- **GIVEN** a running daemon for the home
- **WHEN** the user runs `prometheus chat`
- **THEN** the chat SHALL run as a client of the daemon and SHALL NOT open `state.db` itself

### Requirement: Daemon discovery file and local control channel
The daemon SHALL write `<PROMETHEUS_HOME>/run/gateway.json` (mode 0600) containing `pid`, `instance_id` (random per start), `build_id`, `rpc_url` (loopback WebSocket URL), `control` (path of a Unix domain socket, or a named pipe on Windows, accessible only to the owning user), `rpc_protocol` and `started_at`, SHALL remove it on clean shutdown, and clients SHALL treat a file whose `pid` is not alive as stale.
**Milestone:** M2

#### Scenario: Stale discovery file
- **GIVEN** `gateway.json` names a pid that no longer exists
- **WHEN** a client starts
- **THEN** the client SHALL treat the daemon as not running and SHALL NOT attempt to dial `rpc_url`

### Requirement: Ticket-authenticated loopback attachment
A client SHALL obtain a ticket over the control channel before dialing the RPC WebSocket; a ticket SHALL be single-use, expire after 30 s, and be bound to `instance_id`, the profiles it grants (exactly one, except purpose `profile-admin` which may grant several) and one purpose from exactly `interactive`, `http`, `profile-admin`, `acp`, `eval`. The ticket SHALL travel only as the WebSocket subprotocol `prometheus-ticket.<ticket>` alongside `prometheus-rpc.v<major>`, never in a URL, log line or persisted file, and the daemon SHALL bind the RPC listener to loopback only (`daemon.rpc.bind` in `daemon.toml` SHALL accept only loopback addresses; remote access goes through the relay of `messaging-gateway`).
**Milestone:** M2

#### Scenario: Reused ticket
- **GIVEN** a ticket already used to open one connection
- **WHEN** a second connection presents the same ticket
- **THEN** the daemon SHALL reject the upgrade with HTTP 401 and log the attempt without the ticket value

### Requirement: JSON-RPC framing and error codes
This capability SHALL be the single owner of the local client protocol used by the CLI, TUI, desktop, dashboard and ACP; other capabilities SHALL reference its handshake, error codes and close codes rather than define their own. The local protocol SHALL be JSON-RPC 2.0 with one message per WebSocket text frame (and one message per line on stdio transports such as ACP), and a frame larger than `daemon.rpc.max_frame_bytes` (default 16 MiB) SHALL close the connection with 4413. An unparseable frame SHALL be answered with code `-32700` and `id: null` without closing the connection; a non-object request with `-32600`; non-object params with `-32602`; an unregistered method with `-32601` and message `unknown method: <method>`. Application errors SHALL use exactly these codes mapped from the error taxonomy: `-32001` auth, `-32002` permission_denied, `-32003` not_found, `-32004` conflict, `-32005` budget_exceeded, `-32006` unavailable (including a retiring daemon), `-32007` cancelled, `-32008` protocol, `-32010` invalid_input, `-32011` provider, `-32012` tool, `-32013` sandbox, `-32014` config, `-32099` internal; every error SHALL carry `data: {kind, code, hint?}` where `code` is a stable dotted identifier. Every client listener (RPC, dashboard, ACP bridge) SHALL use exactly these WebSocket close codes: 1000 normal, 1001 shutting down, 1011 internal, 4401 bad credential, 4403 not allowed (Host, Origin, peer or profile), 4404 endpoint disabled on this surface, 4408 heartbeat timeout, 4410 session ended, 4413 frame too large, 4426 protocol version unsupported.
**Milestone:** M2

#### Scenario: Garbage frame
- **WHEN** a client sends the text frame `not json`
- **THEN** the daemon SHALL reply with a `-32700` error and continue serving the connection

#### Scenario: Budget refusal over RPC
- **GIVEN** a session whose budget is exhausted
- **WHEN** a client calls `prompt.submit`
- **THEN** the reply SHALL be `-32005` with `data.kind` equal to `budget_exceeded`

### Requirement: Protocol handshake and versioning
The first request on every connection SHALL be `rpc.hello {client, client_version, protocol: {major, minor}, capabilities: {server_requests, events}}`, where `client` is one of `cli`, `tui`, `desktop`, `dashboard`, `acp`, `eval`, answered with `{server_version, build_id, instance_id, epoch, protocol: {major, minor}, profiles, methods_digest, capabilities, platform_support}` where `profiles` lists the profiles the ticket grants; any other method before `rpc.hello` SHALL fail with `-32008 handshake required`. A different `major` SHALL fail with `-32008 protocol mismatch: client <a.b>, daemon <c.d>; run 'prometheus update'` and the daemon SHALL then close the connection with 4426; the protocol version SHALL be identified everywhere only as `<major>.<minor>` (the `rpc_protocol` value, carried as the `prometheus-rpc.v<major>` subprotocol); minor versions SHALL only add optional fields, methods and events, so a client with a lower minor SHALL keep working.
**Milestone:** M2

#### Scenario: Old client, newer daemon minor
- **GIVEN** a daemon speaking protocol 1.4 and a client speaking 1.2
- **WHEN** the client performs `rpc.hello`
- **THEN** the handshake SHALL succeed and the daemon SHALL NOT send fields or events introduced after 1.2 that the client did not advertise

#### Scenario: Major mismatch
- **GIVEN** a daemon speaking protocol 2.0
- **WHEN** a client speaking 1.7 sends `rpc.hello`
- **THEN** the reply SHALL be error `-32008` naming both versions and the connection SHALL be closed with 4426

### Requirement: Contracts generated from types and checked in
Every RPC method (params and result), server-to-client request (params and result) and event (payload) SHALL be declared in one typed catalog from which the system generates the checked-in `schema/rpc.json` and client bindings for the TypeScript frontends; a method without a declared contract SHALL be impossible to register, and CI SHALL fail when the checked-in schema or bindings differ from a fresh generation. `prometheus rpc schema` SHALL print the schema of the running binary.
**Milestone:** M2

#### Scenario: Stale generated bindings
- **GIVEN** a change to a method's params without regenerating
- **WHEN** CI runs the contract freshness gate
- **THEN** the gate SHALL fail naming each stale file

### Requirement: Catalog index of capability-owned methods and events
The typed catalog SHALL contain at least the following client-protocol methods and events, each with params, result and payload defined by the capability named in parentheses, and no capability SHALL use a different name for the same operation: methods `rpc.hello`, `rpc.ping`, `session.create`, `session.attach`, `session.detach`, `session.list`, `session.resume`, `session.compact`, `session.transcript`, `session.events.since`, `session.context_usage`, `prompt.submit`, `command.dispatch`, `daemon.update` (workspace-architecture); `commands.catalog`, `complete.slash`, `complete.path`, `model.options`, `subagent.list`, `subagent.subscribe` (tui); `approval.respond` (tool-system); `tools.list`, `tools.call` (tool-system); `system.health`, `system.metrics` (observability); `ui.prefs.get`, `ui.prefs.patch` (desktop-app); `pairing.approve` (messaging-gateway); `grants.list`, `grants.revoke`, `egress.set`, `vault.list`, `vault.add`, `vault.remove`, `vault.sources` (security-boundaries); `files.undo`, `files.rollback`, `screen.takeover`, `screen.handback` (execution-environments); `worktree.new`, `worktree.status`, `worktree.list`, `worktree.prune` (git-worktrees); `lsp.status` (lsp-diagnostics); `journey.list` (memory); `cron.blueprints.list`, `cron.blueprints.fill` (scheduled-jobs); `moa.presets.list`, `moa.presets.put`, `moa.presets.delete` (mixture-of-agents); `voice.client_config`, `wake.feed` (voice-mode); `bot.create`, `bot.chat.ensure`, `bot.roster.list`, `bot.meta.get`, `bot.meta.update`, `groups.capabilities`, `groups.log`, `groups.replicate`, `groups.replica_state` (bot-mode); `bot.relay.drain`, `bot.relay.deliver` (agent-interop); `evolve.start`, `evolve.status`, `evolve.list`, `evolve.cancel`, `evolve.choose`, `evolve.proposals.list`, `evolve.proposals.adopt`, `evolve.proposals.reject`, `harness.log`, `harness.diff`, `harness.rollback` (harness-evolution); `experience.list`, `experience.show`, `experience.pin`, `experience.forget`, `experience.restore`, `experience.domains` (experience-library); server requests `approval.request`, `clarify.request`, `secret.request`, `sudo.request` (workspace-architecture › Server-to-client requests); and events `usage.updated`, `health.changed` (observability), `message.delivery`, `adapter.state`, `session.presence` (messaging-gateway), `request.cancel`, `request.resolved` (workspace-architecture), `automation.state` (session-automation), `message.reaction` (desktop-app), `achievement.unlocked` (pets), `wake.detected` (voice-mode) and `local_model.context_grown` (local-models). The bot-screen display stream of `execution-environments` › Bot screen SHALL be a separate RFB-over-WebSocket endpoint on the same listener, authenticated by a single-use 30-second display ticket and using the shared close codes, and the Python bridge protocol (`python-bridge`) and the WIT host interface (`plugin-system`) are separate contracts that SHALL NOT be served on the client protocol.
**Milestone:** M2

#### Scenario: Catalog lists capability methods
- **WHEN** the user runs `prometheus rpc schema`
- **THEN** the output SHALL declare `worktree.new`, `journey.list` and the `automation.state` event with their owning capability recorded in the schema

### Requirement: Strict parameter validation and contract violations
The daemon SHALL validate params against the method contract before invoking the handler, rejecting an undeclared key with `-32602` whose `data` is `{kind: "invalid_input", code: "rpc.unknown_field", path: "<json pointer>"}`, and a missing or mistyped field with `-32602` naming the path. A handler result or event payload that violates its contract SHALL abort the process in debug and test builds and, in release builds, SHALL be logged at ERROR and counted in the `prometheus.rpc.contract_violations` metric.
**Milestone:** M2

#### Scenario: Misspelled param from a skewed client
- **WHEN** a client calls `session.resume` with `{"sesion_id": "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W"}`
- **THEN** the reply SHALL be `-32602` with `data.path` equal to `/sesion_id`

### Requirement: Profile and ownership checks on every session method
Every RPC method that reads or mutates a session, its events, its approvals or its subagents SHALL verify that the session belongs to the profile bound to the connection's ticket and, for mutating methods, that the connection is attached to that session; a violation SHALL fail with `-32002` and SHALL NOT reveal whether the session exists in another profile. This SHALL include `session.events.since`.
**Milestone:** M2

#### Scenario: Cross-profile event replay
- **GIVEN** a connection bound to profile `work`
- **WHEN** it calls `session.events.since` with the id of a session owned by profile `home`
- **THEN** the reply SHALL be `-32002` and no events SHALL be returned

### Requirement: Server-to-client requests
The daemon SHALL send server-to-client requests (approval, clarify, secret entry, sudo, and window bridges) only to connections that declared `capabilities.server_requests = true` in `rpc.hello`; when no attached connection of the session declared it, the request SHALL resolve immediately as unanswered instead of waiting for its deadline. Each request SHALL have an id `srq-<12 hex>`, a deadline (`approvals.timeout_secs`, default 300, for approvals; `daemon.rpc.request_timeout_secs`, default 600, for every other request), and SHALL emit exactly one `request.cancel {id, reason}` event when it times out or is cancelled; a response for a closed id SHALL be dropped.
**Milestone:** M2

#### Scenario: No capable client attached
- **GIVEN** a session whose only attached client omitted `server_requests`
- **WHEN** the agent needs an approval
- **THEN** the approval SHALL resolve as unanswered at once and be treated as a denial

### Requirement: Event sequencing and reconnect replay
Every event frame SHALL carry a per-session monotonic `seq` and the daemon's `epoch` (random per daemon start); the daemon SHALL retain per session the most recent 512 events or 4 MiB (whichever is smaller) for at most 64 sessions and 64 MiB in total, evicting FIFO. `session.events.since {session_id, last_seen}` SHALL return `{events, latest_seq, truncated, epoch, open_requests}` where `truncated` is true when events after `last_seen` were evicted.
**Milestone:** M2

#### Scenario: Reconnect after daemon restart
- **GIVEN** a client holding watermark 97 under epoch `A`
- **WHEN** it calls `session.events.since` on a restarted daemon
- **THEN** the returned `epoch` SHALL differ from `A` and the client SHALL reset its watermark

### Requirement: Connection liveness and backpressure
The daemon SHALL answer `rpc.ping` with `{"ok": true}` directly from the connection's read loop regardless of busy handlers, SHALL run requests that may exceed 50 ms on a bounded worker pool (default 8, `daemon.rpc.workers`) so that one slow request cannot block another, and SHALL close a connection whose single send blocks longer than 30 s. Clients SHALL heartbeat every 15 s and treat 45 s without a reply as a dead connection.
**Milestone:** M2

#### Scenario: Ping during compaction
- **GIVEN** a long compaction running for the session
- **WHEN** the client sends `rpc.ping`
- **THEN** the daemon SHALL reply within 100 ms

### Requirement: Stale daemon build detection
A client whose `build_id` differs from the daemon's SHALL report `daemon is running an older build (<version>); restart with 'prometheus daemon restart'`, and `prometheus daemon restart` SHALL wait for active turns to reach a turn boundary (at most `daemon.restart_drain_secs`, default 120) before replacing the daemon. Retiring daemons SHALL refuse new work with `-32006` and message `daemon is restarting; reconnect to continue`.
**Milestone:** M2

#### Scenario: Binary updated under a running daemon
- **GIVEN** `prometheus update` replaced the binary while the old daemon runs
- **WHEN** the user opens `prometheus tui`
- **THEN** the TUI SHALL show the stale-build notice and SHALL still attach if the protocol major matches

### Requirement: Error taxonomy
Every user-visible error SHALL have exactly one `kind` from: `usage`, `invalid_input`, `not_found`, `config`, `auth`, `permission_denied`, `budget_exceeded`, `provider`, `tool`, `sandbox`, `conflict`, `unavailable`, `protocol`, `cancelled`, `io`, `internal`, plus a stable dotted `code` (for example `config.unknown_key`), a one-line `message`, and an optional `hint` naming the command that fixes it. With `--json`, a failing command SHALL print `{"error": {kind, code, message, hint}}` to stderr. The code catalog SHALL be generated into the documentation and CI SHALL fail when a code is used without a catalog entry.
**Milestone:** M1

#### Scenario: JSON error output
- **GIVEN** `config.toml` contains an unknown key `agent.max_iterationz`
- **WHEN** the user runs `prometheus chat --json -q hi`
- **THEN** stderr SHALL contain an object with `kind` `config` and `code` `config.unknown_key`, and the process SHALL exit 78

### Requirement: Exit codes
The binary SHALL map outcomes to exit codes as: `0` success; `1` the command ran but reported failure (agent error, failing eval tasks, doctor failures); `2` usage error (unknown command, flag or argument); `3` budget exhausted; `4` approval or confirmation required but unobtainable non-interactively; `5` partial result (some items skipped or ungraded); `10` update available (`prometheus update --check` only); `65` invalid input data (for example harness tree validation); `66` not found; `69` unavailable (provider, daemon, runtime, surface missing, or self-update of an install owned elsewhere); `70` internal error; `73` cannot create output; `74` I/O error; `75` temporary failure or lock held (retryable); `76` protocol mismatch; `77` permission denied or strict security posture failure; `78` configuration or credential error; `130` interrupted by SIGINT; `143` terminated by SIGTERM. Every subcommand SHALL use only these codes, and this table SHALL be the single definition that `cli` and `distribution-and-update` restate.
**Milestone:** M1

#### Scenario: Invalid config value
- **GIVEN** `approvals.mode = "of"` in `config.toml`
- **WHEN** the user runs `prometheus chat`
- **THEN** the process SHALL exit 78 and name the key, the bad value and the allowed values

#### Scenario: Ctrl-C during a one-shot query
- **WHEN** the user presses Ctrl-C during `prometheus chat -z "..."`
- **THEN** the turn SHALL be cancelled, the partial turn persisted, and the process SHALL exit 130

### Requirement: Panic isolation and crash reports
A panic inside a session, tool, adapter, plugin host or RPC handler task SHALL be contained to that task, converted to an `internal` error for the affected session or request, and written as a redacted crash report to `<profile home>/logs/crash/<UTC timestamp>-<task>.txt`; the daemon SHALL keep serving other sessions. A panic on the main thread SHALL exit 70 after writing the crash report.
**Milestone:** M1

#### Scenario: Adapter panic
- **GIVEN** the daemon serving two sessions on different adapters
- **WHEN** one adapter task panics
- **THEN** the other session SHALL continue unaffected and a crash report SHALL exist under `logs/crash/`

### Requirement: Cancellation latency
The system SHALL stop a running turn within 500 ms of an interrupt (Ctrl-C, `/stop`, `session.interrupt`), cancelling in-flight model streams and tool processes (process groups receive SIGTERM, then SIGKILL after `tools.kill_grace_secs`, default 5), and SHALL persist the partial turn with an `interrupted` marker.
**Milestone:** M1

#### Scenario: Interrupt during a long shell command
- **GIVEN** the agent is running `sleep 600` through the terminal tool
- **WHEN** the user sends `/stop`
- **THEN** the turn SHALL end within 500 ms and the `sleep` process group SHALL receive SIGTERM

### Requirement: Startup time budget
On the reference CI runner (GitHub-hosted `ubuntu-latest`, x86_64) the system SHALL meet these p95 budgets measured over 30 runs: `prometheus version` ≤ 20 ms; `prometheus chat` with a mock provider until the input prompt accepts keystrokes ≤ 150 ms with no plugins or MCP servers configured; daemon start until `/readyz` returns 200 ≤ 400 ms with one profile. Plugins, MCP servers and managed runtimes SHALL load in the background and SHALL never delay the first prompt. CI SHALL fail when any budget is exceeded or regresses by more than 15% against the main branch baseline; growth under 1 ms (startup median) never counts as a regression, because millisecond-scale runs jitter by more than 15% on a shared runner.
**Milestone:** M1

#### Scenario: Slow MCP server
- **GIVEN** an MCP server configured that takes 5 s to start
- **WHEN** the user runs `prometheus chat`
- **THEN** the prompt SHALL be ready within the 150 ms budget and the MCP tools SHALL become available for the next session snapshot or by `/reload-mcp --now`

### Requirement: Memory and size budgets
On the reference runner the system SHALL stay within: idle daemon with one profile and no sessions ≤ 60 MiB RSS; each additional idle session ≤ 8 MiB RSS excluding model context buffers; `prometheus chat -z` peak ≤ 120 MiB RSS excluding child processes; stripped release binary ≤ 80 MiB including the embedded dashboard assets. CI SHALL enforce these as gates with the same 15% regression rule as startup, where growth under 1 MiB (RSS) or 4 MiB (binary size, 5% of its budget) never counts as a regression.
**Milestone:** M1

#### Scenario: Binary too large
- **WHEN** a change grows the stripped Linux x86_64 binary to 82 MiB
- **THEN** the size gate SHALL fail

#### Scenario: Small binary growth from a near-empty baseline
- **WHEN** a change grows the stripped binary from 0.62 MiB on main to 1.21 MiB
- **THEN** the size gate SHALL pass, because the growth is under the 4 MiB floor even though it exceeds 15%

### Requirement: Repository health gates
The repository SHALL provide one command, `scripts/check`, that runs every blocking CI gate locally with CI's pinned tool versions: formatting, lints with warnings denied, the full test suite, `openspec validate --all --strict`, generated-artifact freshness (config schema, CLI reference, error catalog, and from M2 the RPC schema `schema/rpc.json` and its bindings), dependency license allowlist (MIT, Apache-2.0, BSD-2/3-Clause, ISC, MPL-2.0, Zlib, Unicode-3.0) and known-advisory check, and the startup/memory/size budgets. CI SHALL run exactly the same command.
**Milestone:** M1

#### Scenario: Dependency with a disallowed license
- **WHEN** a change adds a dependency licensed GPL-3.0
- **THEN** `scripts/check` SHALL fail naming the dependency and its license

### Requirement: Complexity and unsafe-code ratchet
The health gates SHALL enforce per function a cognitive complexity ≤ 15 (≤ 30 in parser and protocol-codec modules listed in `health.toml`), ≤ 120 lines and nesting ≤ 5, and per source file ≤ 1,500 lines; existing exceptions SHALL be listed in `health.toml` with a reason and may only shrink. `unsafe` code SHALL be forbidden outside modules allowlisted in `health.toml` (OS sandbox and FFI), and every `unsafe` block SHALL carry a `// SAFETY:` comment.
**Milestone:** M1

#### Scenario: Function grows past the limit
- **GIVEN** a function not listed in `health.toml`
- **WHEN** a change raises its cognitive complexity to 17
- **THEN** the gate SHALL fail naming the function and the limit

### Requirement: Generated reference documentation
The system SHALL generate the configuration reference (every key, type, default and allowed values), CLI reference, RPC reference and error-code catalog from the same typed definitions the binary uses, and CI SHALL fail when the checked-in documentation differs from a fresh generation, so that no default is stated in two places.
**Milestone:** M1

#### Scenario: Default changed in code only
- **WHEN** a change alters the default of `agent.max_iterations` without regenerating docs
- **THEN** the documentation freshness gate SHALL fail

### Requirement: Doctor
This capability SHALL own `prometheus doctor [<group>] [--profile NAME|--all-profiles] [--fix] [--online] [--strict] [--json]`, which SHALL run all groups, or only `<group>` when given, from exactly `home` (permissions 0700/0600), `config` (validates against schema, reports pending migrations and ignored env vars), `secrets` (store permissions, referenced-but-missing secrets), `store` (`state.db` integrity, FTS consistency, schema version), `daemon` (running, build and protocol match), `sandbox` (enforcement mechanism and strength on this OS), `terminal` (selected terminal backend configuration), `runtimes` (managed Node/CPython/browser presence and manifest hashes), `plugins` (signatures, grants), `observability` (exporter reachability when enabled), `network` (IPv6 reachability, proxy and egress isolation), `providers` (credentials present; reachability only with `--online`) and `security` (the posture report defined by `security-boundaries`); `--strict` SHALL turn every `warn` into a failure and SHALL make the `security` group exit 77 as defined there. Each check SHALL report `ok`, `warn` or `fail` with a hint; the command SHALL exit 1 when any check fails and 0 otherwise, and SHALL make no network request without `--online`.
**Milestone:** M1

#### Scenario: Offline doctor
- **WHEN** the user runs `prometheus doctor` without `--online`
- **THEN** the system SHALL make no outbound network connection

#### Scenario: World-readable secrets store
- **GIVEN** `secrets.toml` with mode 0644
- **WHEN** the user runs `prometheus doctor`
- **THEN** the `secrets` check SHALL fail with a hint to run `prometheus doctor --fix`, and the process SHALL exit 1

### Requirement: Doctor repairs are explicit and enumerated
`prometheus doctor --fix` SHALL perform only these repairs and print each one it performs: tighten home, secrets and database file permissions; remove stale `run/` lock and discovery files whose pid is dead; rebuild the full-text index; apply pending config schema migrations after writing a backup `config.toml.bak.<timestamp>`; reinstall managed runtimes whose hash does not match the manifest. Every other failure SHALL be reported with a hint and left unchanged.
**Milestone:** M1

#### Scenario: Stale lock repaired
- **GIVEN** `run/gateway.lock` left by a crashed daemon
- **WHEN** the user runs `prometheus doctor --fix`
- **THEN** the system SHALL remove the stale files and print `removed stale gateway lock (pid <n> not running)`

### Requirement: RPC debug client (upgrade)
`prometheus rpc call <method> [<json-params>] [--profile NAME]` SHALL perform the handshake against the running daemon, send one request, print the result as JSON and exit 0, or print the error object and exit with the code mapped from its `kind`; `prometheus rpc events [--session ID]` SHALL stream event frames as JSON lines until interrupted.
**Milestone:** M2

#### Scenario: Calling an unknown method
- **WHEN** the user runs `prometheus rpc call sesion.list`
- **THEN** the system SHALL print the `-32601` error naming `sesion.list` and exit 2

### Requirement: Machine-readable output contract
Every listing or status subcommand SHALL accept `--json` and emit a single JSON document whose top-level `schema` field names a versioned shape (`prometheus.<noun>.v<major>`); a breaking change to a shape SHALL bump its major version and the generated CLI reference SHALL list every shape.
**Milestone:** M1

#### Scenario: Scriptable session list
- **WHEN** the user runs `prometheus sessions list --json`
- **THEN** the output SHALL be one JSON object with `schema` equal to `prometheus.sessions.v1`

### Requirement: IPv6 reachability check and IPv4-only outbound
The `network` doctor group SHALL, when the host has a global IPv6 route, attempt with `--online` a dual-stack connection to the configured providers' hosts with a 2 s IPv6-only probe and a 2 s IPv4-only probe, and SHALL warn `IPv6 route present but unreachable (blackhole); set network.force_ipv4 = true` when IPv6 fails and IPv4 succeeds; without `--online` it SHALL report only whether an IPv6 route exists. `network.force_ipv4` (default false) SHALL make every outbound client of that ProfileScope (providers, MCP over HTTP, web tools, adapters, OTLP, update and runtime downloads) resolve and connect over IPv4 only, without affecting loopback listeners.
**Milestone:** M1

#### Scenario: Blackholed IPv6
- **GIVEN** a host with a global IPv6 route that drops packets and working IPv4
- **WHEN** the user runs `prometheus doctor network --online`
- **THEN** the check SHALL warn and name `network.force_ipv4 = true`

#### Scenario: Forced IPv4
- **GIVEN** `network.force_ipv4 = true`
- **WHEN** a provider host resolves to both AAAA and A records
- **THEN** the connection SHALL be made only to the A record addresses

### Requirement: Embeddable client SDKs
The project SHALL publish client libraries over the versioned RPC protocol — a Rust crate, a Python package and a TypeScript package, all generated from or tested against `schema/rpc.json` — offering `Client.connect(home?, profile?)` (attaching to or starting the daemon through the ticketed loopback flow), `client.sessions.create(toolsets?, model?, system_prompt_overlay?, ephemeral?)`, `session.prompt(text, attachments?)` returning a stream of the protocol's events and a final result with usage, `session.interrupt()`, approval and clarify callbacks, and `session.history()` for multi-turn use; each SDK release SHALL declare the `rpc_protocol` major versions it supports and SHALL fail `connect` with a protocol-mismatch error (the `-32008` handshake error) rather than proceed against an unsupported daemon. An embedded session SHALL be subject to the same capability grants, approvals and budgets as any other client, and `ephemeral = true` SHALL keep its transcript out of the session store and trajectories.
**Milestone:** M3

#### Scenario: Python embedding
- **GIVEN** a running daemon and the Python SDK
- **WHEN** a script runs `Client.connect().sessions.create(toolsets=["file"]).prompt("list files")`
- **THEN** it SHALL receive `tool.call`, `tool.result` and `assistant.message` events followed by a final result with usage totals

#### Scenario: Old SDK against a new protocol
- **GIVEN** an SDK supporting protocol major 1 and a daemon speaking major 2
- **WHEN** `Client.connect()` runs
- **THEN** it SHALL raise a protocol-mismatch error and send no session request

### Requirement: File-descriptor soft limit
On POSIX hosts every `prometheus` process SHALL at startup raise its open-file soft limit to `process.nofile_soft_limit` (in `daemon.toml`, default 4096; `0` disables) or to the hard limit if that is lower, never lowering an already higher soft limit, and SHALL log at WARN when the resulting limit is below the configured value; child processes SHALL inherit the raised limit.
**Milestone:** M1

#### Scenario: Low default limit raised
- **GIVEN** a shell with `ulimit -Sn 256` and a hard limit of 65536
- **WHEN** the daemon starts with default configuration
- **THEN** its soft open-file limit SHALL be 4096

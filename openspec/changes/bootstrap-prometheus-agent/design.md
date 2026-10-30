# Design: Bootstrap Prometheus Agent

## Context

Greenfield Rust project. Behavior is derived from the Hermes Agent baseline specs (23 capabilities, ~690 requirements, captured from Hermes `main` at 088f657ecf) and from Harness² specs (task-level recursion, experience library, output selection, session cache). See `proposal.md` for motivation. The Hermes baseline also surfaced ~120 doc/code inconsistencies; where they reveal a design weakness, Prometheus resolves it here rather than copying it (see "Lessons from Hermes").

## Goals / Non-Goals

**Goals:**
- One static binary per target that provides every surface (CLI, TUI, gateway, dashboard backend, ACP server, MCP server), with the desktop app as a thin shell over it.
- Same mental model as Hermes (profiles, toolsets, skills, memory, gateway, cron) so Hermes users and their data migrate in one command.
- Safe-by-default extensibility: nothing third-party runs in-process with ambient authority.
- Measurable self-improvement: harness changes are proposed, probed and selected under a fixed budget, never using grader answers.

**Non-Goals:**
- Source or ABI compatibility with Hermes Python internals (only data, skill and plugin-protocol compatibility via `python-bridge`).
- Training or fine-tuning models (weights stay fixed; Prometheus improves the harness around them).
- Shipping every messaging platform in M2: M2 ships Telegram, Discord, Slack, Email, Matrix, Signal, WhatsApp and Webhook in the binary; every other Hermes platform (WhatsApp Cloud, Teams, Google Chat, Mattermost, DingTalk, Feishu, WeCom, Weixin, BlueBubbles, Photon, QQ, Yuanbao, LINE, SMS, ntfy, IRC, Raft, Buzz, SimpleX, Home Assistant, msgraph webhook) is still specified and ships as a first-party signed WASM adapter (or managed sidecar) in the catalog by M3.

## Decisions

### D1. Workspace and binary
Cargo workspace, edition 2024, MSRV pinned in `rust-toolchain.toml`. Crates (`crates/`): `pmt-core` (types, errors, ids), `pmt-config`, `pmt-store`, `pmt-providers`, `pmt-agent` (turn loop, prompt, compression), `pmt-tools`, `pmt-sandbox`, `pmt-skills`, `pmt-memory`, `pmt-plugins` (wasmtime host), `pmt-pybridge`, `pmt-mcp`, `pmt-gateway`, `pmt-adapters-*`, `pmt-cron`, `pmt-kanban`, `pmt-evolve`, `pmt-eval`, `pmt-obs`, `pmt-tui`, `pmt-web`, `pmt-acp`, `pmt-migrate`, and the `prometheus` binary crate. `apps/desktop` is a Tauri 2 app; `web/` is the dashboard SPA (TypeScript, built and embedded into the binary).
*Why:* one binary removes Hermes' installer/PM complexity; crates give compile-time module boundaries in place of Hermes' "facade + siblings" convention. *Alternative:* multiple binaries — rejected, it reintroduces version skew between surfaces.

### D2. Async runtime and process model
Tokio multi-threaded runtime. One **gateway daemon per Prometheus home** owns sessions, agents and schedulers; CLI-in-TTY, TUI, desktop, dashboard and ACP are clients over a local WebSocket (loopback, ticket-authenticated) using a versioned JSON-RPC 2.0 protocol whose schema is generated from Rust types (`schemars`) and checked in. A plain `prometheus chat` can also run the agent in-process without a daemon.
*Why:* Hermes converged on "everything attaches to the gateway" late (TUI stdio → WebSocket) and its docs drifted; Prometheus starts there. *Alternative:* per-surface in-process agents — rejected (duplicate state, profile leaks).

### D3. Profile scope is a value, not ambient state
Every operation that touches profile data takes an explicit `ProfileScope` (home path, secrets handle, config snapshot). No global/env reads of profile state after startup; the process env is read once into the launch config. *Why:* Hermes needs HX002/HX004/HX005/HX012 lint rules to catch default-profile leaks; making scope a required parameter makes the leak unrepresentable.

### D4. Configuration and secrets
`config.toml` per profile, typed with serde + JSON Schema export, with `schema_version` and forward migrations. Secrets live in a separate secrets store (`secrets.toml`, mode 0600, or OS keychain via `keyring`) and are the only values that may come from environment variables (`PMT_SECRET_*` or provider-native names like `OPENAI_API_KEY`). Non-secret env vars are not read except `PROMETHEUS_HOME` and `RUST_LOG`/`PMT_LOG`. Hermes `config.yaml` is accepted by `hermes-migration` only.
*Why:* Hermes surfaced ~10 non-secret `HERMES_*` env reads and `.env` holding non-secrets despite its own rule. *Alternative:* keep YAML — rejected, TOML is the Rust-native typed format and avoids YAML's `off`→`false` approval bug class.

### D5. Storage
SQLite (via `rusqlite`, bundled, FTS5 + trigram tokenizer) in WAL mode, one `state.db` per profile, `0600`. Migrations are embedded and versioned; a startup integrity check quarantines a corrupt file and rebuilds FTS. Large blobs (media, attachments, trajectories) go to a content-addressed store `blobs/<sha256>`.
*Alternative:* sled/redb — rejected, FTS5 and external inspectability matter more than pure-Rust.

### D6. Providers
Provider adapters implement one trait over three wire formats: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages (plus Gemini native and Bedrock via plugins). Streaming is normalised into one internal event stream. Prompt caching (Anthropic `cache_control`, OpenAI prompt cache keys) is a first-class concept on the request type.

### D7. Plugins: WASM components + MCP + Python bridge
- **WASM**: wasmtime with the Component Model; plugin interfaces are WIT worlds (`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`, `context-engine`, `terminal-backend`, `ui-panel`). Each plugin declares capabilities (network hosts, filesystem paths, secrets by name, LLM access, tools it may call) in `plugin.toml`; the host grants only what the user approved. Fuel + epoch interruption bound CPU; memory is capped.
- **MCP**: `rmcp` client for external MCP servers (stdio + streamable HTTP) and an MCP **server** mode exposing Prometheus tools/skills to other agents.
- **Python bridge**: a managed CPython (uv-provisioned) sidecar process speaking JSON-RPC over stdio, hosting Hermes-style Python plugins and skill scripts with a compatibility shim of the Hermes `PluginContext` API. Out-of-process (not PyO3-embedded) so a plugin crash or GIL stall cannot take down the agent and so it can be sandboxed like any child process.
- Plugin kinds in `plugin.toml`: `wasm`, `mcp`, `python` (Hermes `plugin.yaml` is translated to kind `python`). Additional WIT worlds: `delegation-router`; `computer-use-backend`; language packs are no-code `wasm` plugins with an empty `worlds` list; external kanban worker lanes are sandboxed commands declared under `[[kanban_lanes]]` in `plugin.toml` (no dedicated world); `platform-adapter` supports inbound HTTP routes on the shared listener and long-lived outbound connections; provider plugins get an OAuth PKCE hook where the host runs the flow and the plugin receives only an opaque credential handle.
*Why:* Hermes plugins run in-process with full authority; the security spec had to scope plugin malice out. *Alternative:* native dylibs — rejected (no sandbox, ABI churn).

### D8. Execution sandboxing
Local commands run under an OS sandbox profile derived from the session's capability set: Landlock + seccomp (Linux), Seatbelt (macOS), AppContainer/Job Objects (Windows). Container backends use rootless Podman/Docker with `--cap-drop ALL`, `no-new-privileges` (never silently dropped), read-only root and tmpfs scratch. Remote backends (SSH, Modal, Daytona, E2B/Vercel-style) are plugins over the `terminal-backend` WIT world.

### D9. Self-improvement (from Harness²)
A **harness** is the editable tree around the fixed model: system prompt, guardrails, memory policy, tool guidance, subagent definitions, skills, scripts, hooks. `harness-evolution` runs Propose–Probe–Compose in `parallel` (width) or `sequential` (depth) mode with budget k → R=4k rollouts, P=3k improver sessions; a blind selector picks one final among the k finals. It is **answer-free**: no rubric, reference answer or grader verdict may reach improvers/selectors/reflectors. `experience-library` stores adopted harness trees + one distilled lesson per task per domain. Opt-in per profile; adopted harness changes apply from the next session only (prompt-cache invariant). Evolution only *proposes*: adoption needs user approval or an explicit policy, and changes to scripts, hooks or subagent definitions always need approval. Mode `both` yields one proposal per mode and at most one is adopted. The library is per domain (the mode is recorded on each entry). A harness can never widen capabilities, egress, secrets or approval modes. The answer-free boundary also applies to `prometheus eval`.

### D10. Replay, evals and observability
Every model call is content-addressed (hash of the canonical request, excluding transport fields, streaming flags and cache-control markers) and optionally cached; the same prefix hash drives the `cache.unexpected_break` detector; trajectories use one JSONL grammar shared by all surfaces, exported to OTLP. `prometheus eval` runs suites against live or replayed providers; a deterministic mock provider makes the full pipeline testable offline (the Harness² "local demo" idea).

### D11. Distribution
`cargo-dist` builds signed artifacts (macOS universal, Linux gnu/musl x86_64/aarch64, Windows x86_64/aarch64, Android/Termux aarch64), Homebrew tap, shell/PowerShell installers, container image, Nix flake. Self-update verifies a minisign/Sigstore signature before swapping the binary atomically. Managed external runtimes (Node for browser automation, uv + CPython 3.11–3.13 for the bridge, Chromium) are installed lazily into `<home>/runtimes/` with hash-pinned manifests. An install receipt records the owner (`self`, `package-manager`, `image`, `bundle`, `dev`) and channel; self-update of an install owned elsewhere is refused with exit 69 naming the owner. Canary builds are semver prereleases `vX.Y.Z-canary.<UTC timestamp>`. Additional channels: WinGet and offline air-gapped bundles. Plugin and catalog signing keys are embedded in the binary; catalog installs require signatures unless `--allow-unsigned` is given and recorded.

### D12. Frontends
TUI: `ratatui` + `crossterm`. Desktop: Tauri 2 (system webview; smaller and fewer privileges than Electron) reusing the dashboard SPA components. Dashboard: `axum` serving the embedded SPA, loopback by default.

### D13. Home layout
Every profile, including `default`, lives at `<PROMETHEUS_HOME>/profiles/<name>/` (`config.toml`, `secrets.toml`, `grants.toml`, `profile_id` (the profile's stable `prof_<ULID>` id, kept across renames, which `store_meta.profile_id` binds `state.db` to), `state.db`, `blobs/`, `skills/`, `harness/`, `trajectories/`, `cache/`, `scripts/`, `snapshots/`, `python/`). The home root holds only machine-wide state: `active_profile`, `daemon.toml` (daemon-wide settings: listeners, ports, relay, all-adapters-down policy), `run/` (runtime records: pid + start time, never argv scanning), `logs/`, `runtimes/` (hash-pinned managed runtimes), `updates/` (install receipt, update receipts), `kanban/` (boards are deliberately shared across profiles), `backups/` (whole-home backup archives), `models/` (machine-wide local model library; `[local_runtime]` lives in `daemon.toml` because the inference server is shared across profiles), `ui/` (machine-wide UI preferences and dashboard themes), `cache/` (resumable downloads). Per-profile directories beyond the list above: `plugins/`, `plugin-data/`, `hooks/`, `themes/`, `backups/` (pre-migration copies), `evolve/`, `experience/`, `vault/`, `locales/`, `logs/`, `workspace/`, `skill-bundles/`. Settings that are inherently machine-wide (`daemon.*`, `gateway.http.*`, `gateway.relay.*`, `update.*`, `runtimes.*`, `process.*`, `local_runtime.*`) live in `daemon.toml`. This removes Hermes' root-vs-default asymmetry.

### D14. Grants and approvals
`<profile>/grants.toml` (0600) is the single store of permanent approvals and capability grants; agent-triggered writes never touch `config.toml`. Approval modes are typed enums; dismissal, detachment or `approvals.timeout_secs` (default 300) means deny. Gateway-owned secrets (bot tokens, dashboard/relay credentials, tickets) are surface-only and never grantable to tools, children, skills or plugins. Sandboxed children reach the network only through a Prometheus-owned loopback egress proxy that resolves DNS itself; `security.sandbox.allow_degraded` (typed feature list) is the only way to weaken OS hardening.

### D15. Tool descriptors
Every tool declares `effect` (`none` | `workspace` | `external`), `idempotent` (bool, default false), resource claims (read/write paths, exclusive resources, `interactive`) and required capabilities. The turn journal re-runs only `effect = none` or `idempotent = true` calls on crash-resume (others get an "outcome unknown" result); the parallel planner schedules by resource claims; harness-evolution rollouts replace `effect = external` tools with recorders.

### D16. Credentials and subscriptions
Provider auth kinds are `api_key`, `oauth`, `token_mint` and `external_process`. OAuth tokens live in the OS keychain or secrets store behind a cross-process refresh lock (single-use rotating refresh tokens). Borrowed CLI logins (Claude Code, Codex CLI) are read and refreshed in place — the one explicit exception to "all secrets in our store" — and `auth.adopt_external_logins = false` disables them. Every model or paid tool call records a `billing_source` (`subscription:<provider>` | `api_key:<provider>` | `keyless:<vendor>` | `local`); `keyless` sources are only ever used after explicit opt-in.

### D17. Storage details
Built-in memory is rows in `state.db` (core tier in the frozen prompt snapshot, archival tier via recall); Markdown is an edit/export view. `usage_ledger` is append-only and keyed by `root_session_id`. Each binary declares the maximum storage schema it can read; the first open after an update takes a pre-migration snapshot so `prometheus update --rollback` is safe. `store_meta.profile_id` binds a store to its profile.

## Shared contracts (canonical)

Specs reference these instead of redefining them.

- **Client protocol owner:** `workspace-architecture`. First request `rpc.hello`; JSON-RPC 2.0 error codes from the `workspace-architecture` table (`-32601` unknown method, `-32602` invalid params, `-32008` handshake required / protocol major mismatch, plus the fixed `-320xx` application range). WebSocket close codes on every listener: 1000 normal, 1001 shutting down, 1011 internal, 4401 bad credential, 4403 not allowed (Host/Origin/peer/profile), 4404 endpoint disabled, 4408 heartbeat timeout, 4410 session ended, 4413 frame too large, 4426 protocol version unsupported. Schema checked in at `schema/rpc.json`; TypeScript bindings generated from it.
- **Exit codes (every subcommand):** 0 success; 1 failure; 2 usage error; 3 budget exhausted; 4 approval or confirmation required but unobtainable non-interactively; 5 partial result (items skipped/ungraded); 10 update available (`update --check` only); 65 invalid input data (e.g. harness tree validation); 66 not found; 69 unavailable (provider, daemon, runtime, or self-update for an install owned elsewhere); 70 internal error; 73 cannot create output; 74 I/O error; 75 temporary failure / lock held; 76 protocol mismatch; 77 permission denied or strict security posture failure; 78 configuration or credential error; 130 SIGINT; 143 SIGTERM.
- **Config key naming:** `snake_case`, durations suffixed `_secs` or `_ms`, sizes `_bytes`. Key ownership: each capability's spec owns its section.
- **Ports and channels:** all listeners are daemon-wide and configured in `daemon.toml`: dashboard `127.0.0.1:9120` (the webapp is served on the same listener), remote daemon attachment `9121`, OpenAI-compatible API server `8642` (off by default, fail-closed without a per-profile key), shared webhook listener `8644`, subscription proxy `8645`, A2A `9900` (off by default), egress proxy `9090` (loopback); release channels `stable` and `canary` (desktop updater uses the same).
- **Interface selection:** `ui.interface = "tui" | "line"` (default `tui` on an interactive TTY, `line` otherwise; M1 ships `line` only).
- **Busy input:** one key `busy.input_mode = "queue" | "steer" | "interrupt"` (default `queue`) with per-platform overrides under `gateway.platforms.<platform>.busy`.
- **Approvals:** event `approval.request`, RPC `approval.respond {id, decision: once|session|always|deny, accepted_hunks?, replacement?}`.
- **Hook names:** the closed list in `plugin-system` is authoritative.
- **RPC names:** the catalog-index requirement in `workspace-architecture` is the authority for every RPC method and event name.
- **Bot Chat delivery target:** `bot_chat[:<profile>]`.
- **Billing labels:** `subscription:<provider>/<label>`, `api_key:<provider>/<label>` (label omitted when the credential has none); ledger columns `purpose`, `purpose_detail`, `service_tier`.
- **Managed runtimes:** node, cpython, uv, chromium, lightpanda, doc-converter, git-bash (Windows), cua-driver, llamacpp, lsp servers, Bitwarden/1Password CLIs, adapter bridges; container backends use the `prometheus-sandbox` image (and `prometheus-sandbox-desktop` for computer use).
- **Deliberate differences from Hermes:** MCP sampling enabled only for `trust = "full"` servers; gateway hook directories need a grant; `gateway.trust_env = false`; Telegram pin-during-turn opt-in; default wake phrase "hey prometheus"; plain HTTP refused for remote desktop connections; migrated adapters and catalog plugins arrive installed but disabled.
- **Ids:** sessions `ses_<ULID>`, messages `msg_<ULID>`; identity file `SOUL.md`; essential skill `prometheus-agent`; todo tool `todo`.

### D18. Feature parity is tracked, not assumed
`docs/hermes-parity.md` maps every inventoried Hermes feature (docs pages, CLI and slash-command registries, toolsets, platforms, providers, plugins, config sections) to the Prometheus requirement that specifies it; only a short, reasoned list is N/A (Hermes-internal dev tooling, retired commands, non-secret env vars replaced by typed config). A Hermes feature without a mapping is a spec bug. Deliberate behavioural differences (e.g. MCP sampling only for fully trusted servers, `gateway.trust_env = false`, plain HTTP refused for remote desktop connections) are recorded in the owning spec.

## Lessons from Hermes (resolved by design)

| Hermes finding | Prometheus decision |
|---|---|
| Non-secret `HERMES_*` env vars and non-secrets in `.env` | D4: typed config only; env is secrets-only |
| YAML `off` parsed as boolean flips approval semantics | D4: TOML with typed enums; unknown values fail validation |
| Default-profile leaks from ambient state | D3: explicit `ProfileScope` |
| Docs/code drift on defaults (turn limit, compression ceiling, delegation caps) | Defaults defined once in the config schema; docs generated from it |
| Contracts declared but unenforced (RPC registration, busy policies, hook names) | Typed registries; unknown hook/command/RPC names are compile- or load-time errors |
| In-process plugins with full authority | D7: WASM capability grants; Python out-of-process |
| Sandbox hardening dropped silently for compatibility | D8: hardening downgrades require explicit config and are logged at WARN on every start |
| Usage totals miss child/continuation sessions | Usage ledger keyed by root lineage (see `observability`) |
| Configured-but-unregistered providers fall back in one category, fail in another | Unknown provider/engine/backend names fail closed with exit 78 in every category |
| Security scanners fail open on internal error in some paths | Scanner errors fail closed everywhere; override is an explicit, recorded flag |
| `execute_code` falls back to every sandbox tool when none are enabled | Programmatic tool calling runs under the caller's grant; an empty intersection is refused |
| Three client wire variants and two close-code sets | One protocol owner and one code table (see Shared contracts) |

## Risks / Trade-offs

- [Parity scope is very large] → Milestones M1–M3 with independently shippable cuts; tasks.md is grouped by milestone.
- [WASM component tooling maturity] → Pin wasmtime; MCP and Python bridge remain as escape hatches.
- [OS sandbox differences across platforms] → Capability model is the contract; enforcement strength per OS is reported by `prometheus doctor` and documented.
- [Self-improvement cost] → Opt-in, hard budget ledger, cached sessions, never runs inside a user turn.
- [Hermes plugin compatibility via bridge is partial] → Bridge declares a supported `PluginContext` subset; unsupported calls fail with a clear error naming the API.

## Migration Plan

Users run `prometheus migrate from-hermes [--hermes-home PATH] [--profile NAME|--all]` (read-only on the source). Rollback is deleting the Prometheus home; Hermes is untouched.

## Open Questions

- Final public names for the managed cloud scheduler and relay services (behavior is specified; endpoints are configuration).
- Which additional messaging platforms ship as first-party WASM adapters in M3 (contract is fixed by `platform-adapters`).

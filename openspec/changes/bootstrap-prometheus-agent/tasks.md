# Tasks

Groups are ordered by milestone (M0 scaffolding, then M1, M2, M3) and by dependency inside each milestone. Every task names the capability it implements as `(spec: <capability>)`. Tests and docs ship in the same group as the code they cover; each milestone ends with an exit-criteria group that only runs integration checks.

## 1. M0 Cargo workspace and toolchain

- [ ] 1.1 Create the Cargo workspace (edition 2024) with `rust-toolchain.toml` pinning the MSRV, plus empty library crates `pmt-core`, `pmt-config`, `pmt-store`, `pmt-providers`, `pmt-agent`, `pmt-tools`, `pmt-sandbox`, `pmt-skills`, `pmt-memory`, `pmt-obs` and the `prometheus` binary crate laid out as D1 describes; verify that `cargo build --workspace` and `cargo test --workspace` succeed on a clean checkout (spec: workspace-architecture)
- [ ] 1.2 Add placeholder crates for later milestones (`pmt-plugins`, `pmt-pybridge`, `pmt-mcp`, `pmt-gateway`, `pmt-cron`, `pmt-kanban`, `pmt-evolve`, `pmt-eval`, `pmt-tui`, `pmt-web`, `pmt-acp`, `pmt-migrate`), each with a crate-level doc comment that states what it owns; verify `cargo metadata` lists every D1 crate and `cargo doc --workspace --no-deps` builds (spec: workspace-architecture)
- [ ] 1.3 Make the `prometheus` binary print `prometheus <version>` and install a `pmt` alias (a hard link or copy in dev installs); verify that `cargo run -- --version` and the alias print the same identity line (spec: cli, spec: workspace-architecture)
- [ ] 1.4 Set the workspace lint policy: `#![forbid(unsafe_code)]` by default, clippy pedantic set, warnings denied in CI, and a `deny.toml` license allowlist (MIT, Apache-2.0, BSD-2/3-Clause, ISC, MPL-2.0, Zlib, Unicode-3.0) with advisory checks; verify that `cargo clippy --workspace -- -D warnings` and `cargo deny check` pass (spec: workspace-architecture)

## 2. M0 CI and repository health gates

- [ ] 2.1 Write `scripts/check`, the single entry point that runs fmt, clippy, tests (`cargo nextest`), `cargo deny`, `openspec validate --all --strict` and the generated-artifact freshness check with pinned tool versions; verify that it exits 0 on a clean tree and non-zero when a file is unformatted (spec: workspace-architecture)
- [ ] 2.2 Add a GitHub Actions workflow that runs only `scripts/check` on the Tier 1 matrix (macOS arm64/x86_64, Linux gnu/musl x86_64/aarch64) and a build-only job for the Tier 2 targets (Windows x86_64/aarch64, Android aarch64), and fold the existing `openspec-validate.yml` into it; verify that a test PR shows every job green and that a deliberate clippy warning fails CI (spec: workspace-architecture)
- [ ] 2.3 Implement the complexity and unsafe ratchet: `health.toml` holds exceptions, the code-health tool enforces cognitive complexity ≤ 15 (≤ 30 for listed parser/codec modules), ≤ 120 lines per function, nesting ≤ 5 and ≤ 1,500 lines per file, and allows `unsafe` only in allowlisted modules with `// SAFETY:` comments; verify with fixture crates that a function over budget fails, an exception that grows fails, and an `unsafe` block without a comment fails (spec: workspace-architecture)
- [ ] 2.4 Add the performance-gate harness (hyperfine/RSS sampler) that records the startup, memory and binary-size budgets against a stored main-branch baseline and fails on a budget breach or a regression above 15%; verify by running it on the stub binary and by injecting a deliberate 200 ms sleep, which must fail (spec: workspace-architecture)
- [ ] 2.5 Add `CONTRIBUTING.md` and `SECURITY.md` skeletons (the trust model text comes in group 8) that document `scripts/check`, the pre-push hook and the health rules; verify that the commands they quote run as written (spec: workspace-architecture, spec: security-boundaries)

## 3. M0 Schema and reference-doc generation pipeline

- [ ] 3.1 Create an `xtask` crate with `cargo xtask gen` that writes the config JSON Schema (`schemars`), the CLI reference (clap), the error-code catalog and, once it exists, `schema/rpc.json` plus its TypeScript bindings into checked-in paths; verify that running it twice yields no diff (spec: workspace-architecture)
- [ ] 3.2 Add `cargo xtask gen --check` to `scripts/check` so CI fails whenever checked-in generated artifacts differ from a fresh generation; verify by hand-editing a generated file and seeing CI fail (spec: workspace-architecture)
- [ ] 3.3 Set up the documentation site skeleton (`docs/`) whose configuration, CLI and error reference pages are included from generated files only; verify that the docs build and that a test default changed in code appears in the regenerated reference (spec: workspace-architecture)

## 4. M1 Core types, errors and process contracts

- [ ] 4.1 Implement in `pmt-core` the ids (`ses_<ULID>`, `msg_<ULID>`), the error taxonomy and the shared exit-code table (0,1,2,3,4,5,10,65,66,69,70,73,74,75,76,77,78,130,143) with a mapping from every error kind to an exit code; verify with a table test that every error kind maps to exactly one documented code and that the generated error catalog lists them all (spec: workspace-architecture)
- [ ] 4.2 Implement the launch environment reader, which reads the process env exactly once into a `LaunchEnv` (only `PROMETHEUS_HOME`, `RUST_LOG`/`PMT_LOG` and secret-bearing names) and lists ignored non-secret vars for doctor; verify with a test that a `PMT_FOO=bar` var has no effect and is reported as ignored (spec: workspace-architecture, spec: configuration-and-profiles)
- [ ] 4.3 Implement the typed-registry helper (unknown name → typed error listing the available names, exit 78) used by providers, backends, toolsets, engines and hooks; verify with unit tests that unknown names never fall back (spec: workspace-architecture)
- [ ] 4.4 Implement panic isolation (catch at task boundaries, write a crash report under `logs/`, return exit 70) and a cancellation token tree that meets the cancellation-latency requirement; verify with tests that a panicking tool task does not kill the process and that cancellation is observed within the spec bound (spec: workspace-architecture)
- [ ] 4.5 Implement the version/build identity (`prometheus version [--json]` with semver, git sha, target, build profile, storage schema max and protocol version) and the machine-readable output contract (`--json` emits one JSON document on stdout, diagnostics go to stderr); verify with snapshot tests (spec: workspace-architecture, spec: cli)
- [ ] 4.6 Implement runtime records under `run/` (pid plus start time, no argv scanning) and the single-writer storage lock helper; verify with a test that a second writer gets exit 75 and that a stale record with a dead pid is detected (spec: workspace-architecture, spec: cli)
- [ ] 4.7 Implement the startup file-descriptor soft-limit raise (`process.nofile_soft_limit` in `daemon.toml`, default 4096, never lowering a higher limit, WARN when below target, inherited by children) and the `network` doctor group's IPv6 reachability check with the `network.force_ipv4` outbound mode used by every HTTP client; verify that a low default limit is raised and that a blackholed IPv6 route is reported and avoided with `force_ipv4` (spec: workspace-architecture)

## 5. M1 Configuration, profiles and secrets

- [ ] 5.1 Implement home resolution (`--home` > `PROMETHEUS_HOME` > platform default) and the D13 layout created with modes 0700/0600, with every profile, `default` included, under `profiles/<name>/`; verify with temp-home tests that the layout, permissions and `active_profile` are correct (spec: configuration-and-profiles)
- [ ] 5.2 Implement `ProfileScope` (home path, secrets handle, config snapshot) as the required parameter of every profile-data API, with no global profile state; verify with a compile-fail test (trybuild) that store/config access without a scope does not compile (spec: configuration-and-profiles)
- [ ] 5.3 Define the typed `config.toml` schema (serde + schemars) as the single source of defaults for M1 sections, using snake_case keys, `_secs`/`_ms`/`_bytes` suffixes, typed enums and `schema_version`; verify that `cargo xtask gen` regenerates the config reference and that the docs list every M1 key with its default (spec: configuration-and-profiles)
- [ ] 5.4 Implement strict validation (unknown keys and invalid enum values are errors with path and allowed values, exit 78) and the layering (defaults < profile file < `--set`); verify with tests that the YAML-style `off` value is rejected for approval modes and that `--set` overrides are typed (spec: configuration-and-profiles)
- [ ] 5.5 Implement format-preserving atomic config writes (toml_edit, temp file + rename, comments kept) and the config CLI (`config get|set|unset|edit|path|validate`, plus `config explain <key>` showing the value source chain); verify with round-trip tests that comments survive and `explain` names the winning layer (spec: configuration-and-profiles)
- [ ] 5.6 Implement schema versioning with forward migrations and a `.bak.<timestamp>` backup; verify with a fixture `schema_version = 0` file that migrates and keeps a backup, and that a file newer than the binary is refused with exit 78 (spec: configuration-and-profiles)
- [ ] 5.7 Implement the secrets store (`secrets.toml` 0600 or OS keychain via `keyring`), the resolution precedence (env provider-native names / `PMT_SECRET_*` > store) scoped to the profile, and the `secrets set|list|rm` CLI that never echoes values; verify with tests that secrets never appear in config, logs or `--json` output (spec: configuration-and-profiles, spec: security-boundaries)
- [ ] 5.8 Implement profile name rules, the lifecycle commands (`profile list|create|use|rename|delete|show`), safe deletion (confirmation, refusal while the profile is in use) and profile selection at process start (`-p` > `active_profile` > `default`); verify with CLI tests that profiles share no state (islands) and that deleting the active profile is refused (spec: configuration-and-profiles)
- [ ] 5.9 Implement per-use secret resolution (no secret value cached across requests) so a rotated key is used by the next request; verify by rotating a key between two mock-provider calls in one session (spec: configuration-and-profiles)
- [ ] 5.10 Implement whole-home backup (`prometheus backup create` with the `.tar.zst` archive, hashed manifest, SQLite online-backup copies, `--quick` and `--keep` retention), home restore that validates the whole archive before writing anything, and per-profile state snapshots (`prometheus snapshots list|create|restore|prune`, `/snapshot`, reasons `manual|pre-update|pre-restore|pre-migration`); verify that a live database is captured consistently, a corrupt archive writes nothing and a pre-update snapshot restores (spec: configuration-and-profiles)
- [ ] 5.11 Write the configuration and profiles user guide (home layout, secrets vs config, profiles) and verify that every command it quotes passes an automated docs-snippet test (spec: configuration-and-profiles)

## 6. M1 Session storage

- [ ] 6.1 Implement the `pmt-store` open path (only through `ProfileScope`, 0600 files, WAL, bounded readers, checkpoints, `store_meta.profile_id` binding, maximum readable schema) with embedded versioned migrations; verify that opening another profile's db fails and that a newer schema is refused with a clear error (spec: session-storage)
- [ ] 6.2 Implement the session and message tables (session record, source enum, first end reason wins, typed lineage: parent/continuation/fork/child) with transactional message persistence; verify with property tests that a crash mid-turn leaves either all of the turn's rows or none (spec: session-storage)
- [ ] 6.3 Implement the content-addressed blob store `blobs/<sha256>` with reference counting and garbage collection; verify that an unreferenced blob is collected and a referenced one is not (spec: session-storage)
- [ ] 6.4 Implement non-destructive in-place compaction rows, the turn lease and compaction lock; verify with concurrent tests that two compactions on one session serialize and that the original rows stay recoverable (spec: session-storage, spec: context-compression)
- [ ] 6.5 Implement session titles with provenance, lineage numbering (`Title #2`) and title/prefix/id resolution; verify with tests for ambiguous and exact matches (spec: session-storage)
- [ ] 6.6 Implement FTS5 full-text search with the trigram tokenizer, query sanitisation and derived indexes that fail open and rebuild offline; verify that hostile FTS syntax is neutralised and that a dropped index is rebuilt by `doctor --fix` (spec: session-storage)
- [ ] 6.7 Implement the startup integrity check with quarantine of a corrupt file, out-of-band replacement detection and the write-contention policy (busy timeout, then exit 75); verify with a corrupted fixture that it is quarantined and a fresh db is created (spec: session-storage)
- [ ] 6.8 Implement rewind as a soft delete, deletion semantics, retention/pruning/auto-archive, resume/export size guards and usage events keyed by root lineage; verify with tests that rewound messages are hidden but restorable and that retention prunes only eligible sessions (spec: session-storage)
- [ ] 6.9 Implement the integration source tag (integration sessions excluded from continue/resume pickers) and export formats with the save command (redacted by default); verify that `--continue` skips an integration session and that a redacted save contains no secrets (spec: session-storage)
- [ ] 6.10 Implement the `sessions` CLI (`list`, `show`, `search`, `rename`, `export`, `delete`, `prune`, `rewind`) with `--json`; verify with CLI snapshot tests, and document it in the session guide (spec: session-storage, spec: cli)

## 7. M1 Observability: trajectories, logs, usage ledger and budgets

- [ ] 7.1 Implement the trajectory grammar in `pmt-obs` (JSONL event types, `grammar_version`, forward compatibility that skips unknown events) and the canonical text rendering; verify with golden tests that render a recorded trajectory byte-for-byte (spec: observability)
- [ ] 7.2 Implement the trajectory writer (lossless, redacted with the known-secret redactor, retention and size limits); verify that a secret placed in a tool result is redacted in the file and that size caps rotate correctly (spec: observability, spec: security-boundaries)
- [ ] 7.3 Implement structured logs (tracing, JSON files per profile and at the root, rotation at 10 MiB keeping 5 files, `PMT_LOG`/`RUST_LOG`); verify that log records carry session/component fields and that rotation happens at the size limit (spec: observability)
- [ ] 7.4 Implement `prometheus logs` (components, `-n`, `-f`, `--level`, `--session`, `--since`, `--root`, `--json`, printing every file path it reads); verify with CLI tests for each flag and for invalid `--since` handling (spec: observability, spec: cli)
- [ ] 7.5 Implement the append-only `usage_ledger` (per call: tokens by class, `cost_usd` or null, `cost_source`, `billing_source`, root session id) and the pricing rules (provider-reported, then metadata table, then `unpriced`); verify that unpriced calls show as `—` and are counted in `unpriced_calls` (spec: observability)
- [ ] 7.6 Implement totals keyed by root lineage and `prometheus usage` (session, root, day, month views, `--json`); verify that a continuation and a child session roll up to the root (spec: observability, spec: cli)
- [ ] 7.7 Implement the budget engine: pre-call worst-case estimation against every applicable scope (turn, session, root, daily, monthly, unattended), warnings at the configured ratio, `budget.exceeded`/`budget.warning` events, agent-tool writes to budget keys refused, and CLI one-shot exit 3; verify with the scenario tests from both budget requirements (spec: observability, spec: agent-turn-loop)
- [ ] 7.8 Ensure no outbound telemetry exists by default (no network client is constructed unless an opt-in key is set, HTTPS only); verify with a test that runs a full mock session under a deny-all network namespace/mock and observes zero connections (spec: observability, spec: security-boundaries)

## 8. M1 Security boundaries: capabilities, grants and redaction

- [ ] 8.1 Write `SECURITY.md` with the declared trust model, in-scope findings and private reporting channel; verify that the doc names every boundary listed in the spec (spec: security-boundaries)
- [ ] 8.2 Implement the typed capability vocabulary (`fs.read`, `fs.write`, `net`, `process`, `secrets:<name>`, `llm`, `tools:<name>` …) and capability resolution that can only narrow (intersection semantics); verify with property tests that no resolution path widens a set (spec: security-boundaries)
- [ ] 8.3 Implement `grants.toml` (0600) as the single store of permanent approvals and grants, with `prometheus grants list|revoke`; verify that agent-triggered approvals write only to `grants.toml`, never `config.toml` (spec: security-boundaries, spec: tool-system)
- [ ] 8.4 Implement fail-closed grant prompts (dismissal, detach or `approvals.timeout_secs` default 300 → deny); verify with a test that a timed-out prompt denies (spec: security-boundaries)
- [ ] 8.5 Implement the known-secret redactor (every value in the secrets store plus provider token patterns) wired into logs, trajectories, tool results and errors; verify with tests seeded with secrets across all sinks (spec: security-boundaries)
- [ ] 8.6 Implement private and metadata destination blocking (RFC 1918, link-local, cloud metadata endpoints) for URL-taking tools unless granted; verify that `http://169.254.169.254/` is refused (spec: security-boundaries)
- [ ] 8.7 Implement private file mode enforcement and the external-surface fail-closed rule (no listener binds non-loopback without explicit config); verify with tests on created files and a bind attempt (spec: security-boundaries)
- [ ] 8.8 Implement in-process fetch SSRF protection (every redirect hop and resolved address re-checked against the destination policy, DNS failure fails closed) and TLS verification against the platform trust store; verify that a redirect to the metadata endpoint is refused and that a corporate root CA installed in the OS store is trusted (spec: security-boundaries)
- [ ] 8.9 Implement the protected instruction files configuration (built-in patterns plus user patterns whose writes always need approval, even in the most permissive approval mode); verify that an extra pattern still prompts under yolo (spec: security-boundaries)

## 9. M1 Local OS sandbox and execution environment

- [ ] 9.1 Implement in `pmt-sandbox` the capability-to-policy translation and the Linux backend (Landlock fs plus TCP where the kernel supports it, seccomp filter), including denial of credential stores by default; verify with integration tests on Linux CI that a sandboxed `cat ~/.ssh/id_rsa` and a write outside the workspace both fail (spec: security-boundaries, spec: execution-environments)
- [ ] 9.2 Implement the macOS Seatbelt profile generator with the same policy; verify with macOS CI integration tests that mirror 9.1 (spec: security-boundaries)
- [ ] 9.3 Implement the Windows AppContainer + Job Object backend; verify with Windows CI tests (Tier 2) that a write outside the workspace fails, and report strength per feature (spec: security-boundaries)
- [ ] 9.4 Implement the enforcement-strength report (`fs`, `net`, `process`, `syscalls` → `enforced|partial|unavailable`) and `security.sandbox.allow_degraded`, with a WARN logged on every start while degraded; verify that an unavailable feature without `allow_degraded` refuses to run commands (spec: security-boundaries)
- [ ] 9.5 Implement the typed terminal backend registry (`local` built in; `container`/`ssh` names reserved and failing with a "ships in M2" error until group 35), readiness checks and terminal config defaults; verify that an unknown backend exits 78 with the available list (spec: execution-environments)
- [ ] 9.6 Implement the local backend: working-directory resolution, session cwd tracking, terminal argument validation, environment built from an allowlist, and pre-execution guards that fail closed; verify with tests that the parent's secrets are absent from the child env (spec: execution-environments)
- [ ] 9.7 Implement the foreground cap with promotion to background and the server guard, foreground result shaping, spilling full output to a redacted 0600 file, and infrastructure failures returned as degraded results; verify with tests on a long-running command and on a huge output (spec: execution-environments)
- [ ] 9.8 Implement the background process registry (`process` toolset: list, poll, kill, wait) with reaping on session end; verify that no orphan remains after the session closes (spec: execution-environments)
- [ ] 9.9 Implement shell startup file handling (rc files sourced non-interactively with stdin closed and a timeout) and the non-secret environment passthrough allowlist (secret-looking names refused); verify that an rc file that blocks on read does not hang a command and that a secret listed for passthrough is rejected (spec: execution-environments)
- [ ] 9.10 Implement the Windows local shell resolution (Git Bash or PowerShell, never WSL `bash.exe` by accident); verify on Windows CI with a PATH containing only WSL bash (spec: execution-environments)

## 10. M1 Model providers: registry, wire formats and resolution

- [ ] 10.1 Implement the typed, profile-scoped provider registry and the descriptor contract (id, wire format, host rules, auth kinds, base URL, capabilities); verify that unknown and disabled providers fail with typed errors and exit 78 (spec: model-providers)
- [ ] 10.2 Implement the `chat_completions`, `responses` and `anthropic_messages` adapters normalised into one event stream (`text_delta` … one terminal `stop`), plus the wire-format selection order; verify with recorded HTTP fixtures (wiremock) per format and a host-rule test (spec: model-providers)
- [ ] 10.3 Implement requested-provider precedence without ambient env, auto detection, the rule that local endpoints never fall back to remote providers, and `prometheus model explain` for deterministic resolution; verify with resolution table tests (spec: model-providers)
- [ ] 10.4 Implement the model metadata catalog (context window, pricing, capabilities) and capability-aware route validation with a minimum context window; verify that a route lacking tool support for a tool-using session is rejected (spec: model-providers)
- [ ] 10.5 Implement error classification, same-route retries, fallback chain configuration, in-turn fallback activation with turn-scoped restore, and a stable cache prefix across route changes; verify with fault-injection tests (spec: model-providers, spec: agent-turn-loop)
- [ ] 10.6 Implement prompt caching on the request type (Anthropic `cache_control`, OpenAI prompt cache key) and per-call cost accounting into the ledger with `billing_source`; verify with fixture requests carrying the expected cache markers (spec: model-providers, spec: observability)
- [ ] 10.7 Implement auxiliary task routing (title, summary, vision …) where the auto route never guesses another account; verify that the auxiliary route uses only the main credential's provider unless configured (spec: model-providers)
- [ ] 10.8 Implement the `/model` switch (next turn, cache-break notice) and `prometheus model [list|set|info]`; verify with CLI tests (spec: model-providers, spec: cli)
- [ ] 10.9 Implement the built-in provider catalog (every built-in id listed with its descriptor; plan-specific keys preferred over shared keys, with shared-key fallback); verify with catalog table tests for each scenario (spec: model-providers)
- [ ] 10.10 Implement local server detection and context probes (llama.cpp `/props`, Ollama and OpenAI-compatible servers, root URL normalisation, small-context warning) and the context length detection chain (pinned value labelled, probe, metadata, discovered value cached for offline reuse); verify with recorded local-server fixtures (spec: model-providers)
- [ ] 10.11 Implement reasoning effort defaults, per-model overrides (spelling-tolerant matching, clamped downward to what the model supports) and their display in the status line and `model info`; verify with table tests (spec: model-providers)
- [ ] 10.12 Implement the launch-only ad-hoc endpoint (base URL and key from flags, never persisted), user model aliases that can switch provider and credential, and the fallback chain commands (duplicate entries refused); verify with CLI tests that an ad-hoc key never reaches disk (spec: model-providers, spec: cli)

## 11. M1 Provider credentials and subscription sign-in

- [ ] 11.1 Implement credentials bound to their authoritative hosts and `prometheus auth add|list|rm|status` for `api_key`; verify that a key for provider A is never sent to provider B's host (spec: model-providers)
- [ ] 11.2 Implement the subscription/OAuth sign-in registry with device-code and browser PKCE flows (loopback redirect, state check) behind a test IdP; verify with an end-to-end test against a mock OAuth server (spec: model-providers)
- [ ] 11.3 Implement Anthropic Claude Max sign-in (`auth add anthropic --type oauth`, required request identity, `auth.plan_not_eligible` mapping); verify with recorded fixtures for an eligible and an ineligible plan (spec: model-providers)
- [ ] 11.4 Implement OpenAI Codex ChatGPT sign-in (device code by default, PKCE on `localhost:1455` with fallback when the port is busy, `responses` wire with the account id) and `auth import codex-cli`; verify with mock-server tests, including the busy-port fallback (spec: model-providers)
- [ ] 11.5 Implement borrowed CLI logins (Claude Code, Codex CLI) read and refreshed in place, gated by `auth.adopt_external_logins`; verify that `false` ignores them and that refresh rewrites only the original file (spec: model-providers)
- [ ] 11.6 Implement subscription tokens kept only in the secrets store, token refresh under a cross-process lock, and dead-token quarantine; verify with a two-process test that a single-use refresh token is used exactly once (spec: model-providers)
- [ ] 11.7 Implement credential pools (selection strategies) and exhausted-credential cooldowns; verify with tests that a 429'd key is skipped until its cooldown ends (spec: model-providers)
- [ ] 11.8 Document provider setup, including the subscription sign-ins, in the providers guide and verify the quoted commands with the docs-snippet test (spec: model-providers)

## 12. M1 Mock provider

- [ ] 12.1 Implement the built-in `mock` provider (always available, no network, a script of ordered rules matching step index, last user message or last tool result; echo mode; usage ceil(bytes/4); cost 0); verify that two runs of the same script produce byte-identical event streams (spec: replay-and-evaluation)
- [ ] 12.2 Implement mock fault injection (errors, 429, timeouts, truncated streams) used by the retry and fallback tests; verify that each injected fault maps to its classified error (spec: replay-and-evaluation)

## 13. M1 Tool system

- [ ] 13.1 Implement the typed tool registry in `pmt-tools` (descriptor with JSON schema, `effect`, `idempotent`, resource claims, required capabilities), shadowing rejected unless granted, scoped per profile; verify with registry unit tests (spec: tool-system)
- [ ] 13.2 Implement the built-in toolsets and composition (include cycles are load errors, `all` expansion) and validated toolset selection; verify that a cycle is reported with its path (spec: tool-system)
- [ ] 13.3 Implement exposure rules (toolset membership plus capability coverage), availability probes cached per profile with flake grace, a tool surface frozen per session, and descriptions that reference only present tools; verify that changing toolsets mid-session affects only the next session (spec: tool-system)
- [ ] 13.4 Implement the dispatch pipeline and typed result contract, with bounded and sanitized error bodies; verify that a 1 MB error body is truncated to the bound (spec: tool-system)
- [ ] 13.5 Implement parallel and timed execution, the result budget with spillover to files and spillover retention; verify that an oversized result is replaced by a spill reference (spec: tool-system)
- [ ] 13.6 Implement typed approval modes, approval floors that cannot be bypassed, human choices (`once|session|always|deny`) persisted to `grants.toml`, unattended contexts that never wait, and a fail-closed approval channel; verify with the approval scenario tests (spec: tool-system)
- [ ] 13.7 Implement core toolset membership (an unavailable member such as browser leaves the rest of core intact), single-purpose and default-off toolsets (enabled only when named explicitly) and composite toolsets including the `safe` (no execution) and `coding` postures; verify with toolset expansion table tests (spec: tool-system)
- [ ] 13.8 Implement content-level dangerous-command detectors (credential exfiltration and similar patterns feeding the approval floor) and `prometheus approvals test` that runs a command string through the real gate order without executing it; verify that exfiltrating a key is flagged, normal header use is not, and a hardline command is reported as blocked (spec: tool-system)
- [ ] 13.9 Implement the `tools` introspection commands (`tools list|show|toolsets`) and the generated tool-name documentation; verify with CLI snapshots and docs freshness (spec: tool-system, spec: agent-turn-loop)
- [ ] 13.10 Implement `prometheus toolsets list|show` and the `/toolsets` slash listing (built-in, plugin and MCP toolsets, aliases folded, resolved counts, default-off and posture flags, active marks); verify with CLI snapshots (spec: tool-system)

## 14. M1 Core tools: file, web, vision, browser, todo and clarify

- [ ] 14.1 Implement `read_file`, `write_file`, `patch`, `search_files` and `list_dir` through the backend, with read guards, pagination, dedup, write guards and stale-overwrite protection; verify with tests that an edit after an out-of-band change is refused (spec: execution-environments)
- [ ] 14.2 Implement the `read_file` additions (encoding detection including UTF-16, conflict-marker warnings, path suggestions on typos) and protected secret files and device-namespace paths (project `.env` and credential files refused, UNC and device paths blocked); verify with fixtures for each scenario (spec: execution-environments)
- [ ] 14.3 Implement the `patch` tool contract (unique-match rule, ambiguous matches refused, V4A multi-hunk patches reporting the failing hunk) and the `search_files` contract (content and file modes, count mode, limits); verify with golden tests (spec: execution-environments)
- [ ] 14.4 Implement post-write diagnostics (built-in syntax checks after a write, attached to the tool result; a crashing checker is reported without failing the write); verify that an introduced syntax error is reported (spec: execution-environments)
- [ ] 14.5 Implement the `terminal` tool on top of group 9; verify that a tool call runs under the sandbox and returns shaped output (spec: execution-environments)
- [ ] 14.6 Implement the `web` toolset (search/extract via configured provider) with destination safety; verify with a mock HTTP server (spec: tool-system, spec: security-boundaries)
- [ ] 14.7 Implement the `web_search` and `web_extract` tool contracts (limit bounds the result list, backend outage is an error, deterministic windowing of long pages, per-URL failure isolation), extracted full text stored on disk under a cap, and web backend selection precedence per capability; verify with a mock HTTP backend (spec: web-and-media-tools)
- [ ] 14.8 Implement `vision_analyze` (region crops at full resolution, text-only main models routed to the auxiliary vision route) and image input routing shared with the turn loop (native pixels or a text description, SVG rasterised for raster-only routes); verify with mock routes (spec: web-and-media-tools, spec: agent-turn-loop)
- [ ] 14.9 Implement browser automation with lazy Chromium/Node runtime install, session resolution, URL safety, supervision, dialog handling and cleanup; verify with a headless integration test against a local page (spec: execution-environments)
- [ ] 14.10 Implement `browser_exec` (batched sub-procedures over the browser session, available without the terminal toolset); verify against the local test page (spec: execution-environments)
- [ ] 14.11 Implement the agent-level tools `todo`, `clarify` and `session_search` (owned by the turn loop; generic dispatch returns the "must be handled by the agent loop" error); verify with dispatch tests (spec: tool-system)
- [ ] 14.12 Implement the `todo` tool contract (merge updates change status only), the `clarify` contract (question-count cap, timeout behaviour) and the `session_search` tool contract (window around a hit, profile isolation); verify with tool tests for each scenario (spec: agent-turn-loop, spec: session-storage)

## 15. M1 Agent turn loop

- [ ] 15.1 Implement the turn entry contract, typed exit reasons, single active turn per session, and iteration phases in `pmt-agent`; verify with mock-provider tests for each exit reason (spec: agent-turn-loop)
- [ ] 15.2 Implement the layered system prompt (identity `SOUL.md`, context files, skills index, memory snapshot, platform hints) frozen per session, with request-time additions that never touch history; verify with a test asserting the system prompt bytes are identical across turns (spec: agent-turn-loop)
- [ ] 15.3 Implement project context discovery, context file scanning (threat scan) and the size cap; verify with fixtures that an injected instruction file is flagged and an oversized one is truncated (spec: agent-turn-loop)
- [ ] 15.4 Implement context references in user input (`@`-references with line ranges and a hard size limit), their safety rules (credential files and path traversal refused) and completion; verify with fixtures (spec: agent-turn-loop)
- [ ] 15.5 Implement personality overlays (switch deferred to the next session, custom overrides of built-ins), the optional session id in the system prompt, the context file read timeout and the offline prompt size report (JSON breakdown per layer); verify that a hung mount does not block a turn and that the report sums to the request size (spec: agent-turn-loop)
- [ ] 15.6 Implement the single iteration limit, per-agent iteration budget and the budget-exhaustion summary handoff, wired to the budget engine of 7.7; verify the turn ends with the right reason after the handoff (spec: agent-turn-loop)
- [ ] 15.7 Implement tool loop guardrails (repeat detection: warnings in interactive sessions, hard stops in cron, search caps), empty-response and narration guards (one retry of an expensive empty reply) and the wall-clock run budget with a wrap-up notice; verify with mock-provider scripts (spec: agent-turn-loop)
- [ ] 15.8 Implement interrupts and steering with placement that preserves the cache prefix; verify that an interrupt mid-stream ends the turn cleanly and that a steer message is appended after the last tool result (spec: agent-turn-loop)
- [ ] 15.9 Implement transcript repair before each request (orphan tool calls/results), model-call retries and auto-recovery, and the failed-turn boundary row; verify with fault-injection mock scripts (spec: agent-turn-loop)
- [ ] 15.10 Implement dependency-planned parallel tool batches from resource claims, with undeclared tools run sequentially; verify that two reads run in parallel and two writes to one path do not (spec: agent-turn-loop)
- [ ] 15.11 Implement cache-hit reporting and `cache.unexpected_break` detection using the canonical prefix hash, plus step events per iteration into the trajectory; verify that an injected prefix change emits the event (spec: agent-turn-loop, spec: observability)

## 16. M1 Context compression

- [ ] 16.1 Implement context engine selection (built-in default, unknown name exits 78) and the compression config schema; verify config validation tests (spec: context-compression)
- [ ] 16.2 Implement the threshold from the main model's input budget, the small-window floor, the trigger decision with blocking reasons, provider-usage-over-estimate, trigger points and the attempt budget; verify with table tests (spec: context-compression)
- [ ] 16.3 Implement the deterministic recoverable prune, protected head, token-budgeted tail and orphan stubs; verify with golden transcripts (spec: context-compression)
- [ ] 16.4 Implement the summary (budget, handoff shape, iterative update, failure handling) via the auxiliary route, compaction always in place, one compaction at a time, and context-overflow recovery; verify with mock scripts that an overflow error triggers a compaction and a retry (spec: context-compression)
- [ ] 16.5 Implement the cache breakpoint plan and prompt cache TTL, plus the `/compress` manual command; verify request fixtures show the planned breakpoints and that compaction is the only cache break recorded (spec: context-compression)

## 17. M1 Skills

- [ ] 17.1 Implement the SKILL.md parser (agentskills.io frontmatter), directory layout with scanner exclusions, search roots with one precedence order, and read-only bundled skills with `skills eject`; verify against agentskills.io fixture skills (spec: skills)
- [ ] 17.2 Implement explicit trust for project-local skills, platform/env/conditional gates and disabled skills; verify that an untrusted project skill is not indexed (spec: skills)
- [ ] 17.3 Implement the skills index in the system prompt with deferred vs immediate activation (`--now`) that preserves the prompt cache; verify that installing a skill mid-session leaves the prompt unchanged until the next session (spec: skills)
- [ ] 17.4 Implement `skills_list`/`skill_view`, load-time template substitution and inline shell (sandboxed), skill secrets/settings and skill slash commands; verify with fixture skills (spec: skills)
- [ ] 17.5 Implement `skill_manage` validation, the description budget and advisory lint, mutation safety with fail-closed scanning, lookup with wrong-profile hints, autonomous creation and improvement, usage records and profile-scoped state; verify that a scanner error refuses the write (spec: skills)
- [ ] 17.6 Implement skill preload at launch and per-profile auto-load (missing entries skipped), stacking several skills in one message (path arguments not swallowed) and skill bundles (a missing member skipped, not fatal); verify that preloaded skills are in the first request (spec: skills)
- [ ] 17.7 Implement `/learn` (distil a skill or knowledge base from URLs, files or books), `/refine` (on-demand self-improvement review), the skill write-approval review surface for staged writes and `/reload-skills` (rescans roots); verify with mock-provider tests that a staged write applies only on approval and a newly copied skill becomes invocable (spec: skills)
- [ ] 17.8 Implement bundled-skill opt-out and official catalog repair (a blank-slate profile stays blank across upgrades); verify with an upgrade fixture (spec: skills)
- [ ] 17.9 Ship the essential `prometheus-agent` skill and the `skills` CLI (`list|show|enable|disable|eject|lint`); verify CLI snapshots and document skill authoring (spec: skills, spec: cli)

## 18. M1 Memory

- [ ] 18.1 Implement the built-in memory store as rows in `state.db` (typed entries, core and archival tiers, core character budgets) and the frozen system-prompt snapshot refreshed only at compression or a new session; verify that a mid-session write does not change the prompt (spec: memory)
- [ ] 18.2 Implement the `memory` tool (targets, operations, `new_text` alias, `replace` refusal), validation before any write, atomic batches judged on the final state, entry matching, the per-turn retry cap, and exact duplicate adds as no-ops; verify with tool tests for each action and for re-adding a known fact (spec: memory)
- [ ] 18.3 Implement threat scanning of memory content, store enable flags, write approval with pending changes, the rule that background review cannot delete unattended, the nudge interval, and per-turn recall injection as a fenced ephemeral block; verify that recall never persists into history (spec: memory)
- [ ] 18.4 Implement the `memory` CLI (`list|show|edit|export|import`) with Markdown as the edit/export view; verify a round trip export → edit → import (spec: memory)

## 19. M1 CLI line interface

- [ ] 19.1 Implement the clap command tree with global flags (`-p`, `--home`, `--set`, `--json`, `--no-color`, `-v`, `-q`, `-V`), `chat` as the default, and stable exit codes; verify CLI snapshot tests and generated CLI reference (spec: cli)
- [ ] 19.2 Implement `chat` flags, one-shot (`-z`/`--oneshot`) and piped mode, and TTY-first interface selection (`ui.interface`; M1 ships `line`); verify that `echo hi | prometheus chat --provider mock` prints `mock: hi` and exits 0 (spec: cli)
- [ ] 19.3 Implement the single typed slash-command registry, command resolution (prefix, alias, typo hint), precedence for non-built-in commands, and the M1 built-in set; verify that `/help` is generated from the registry (spec: cli)
- [ ] 19.4 Implement busy-policy enforcement (`busy.input_mode`), deferred effects with `--now`, destructive-command confirmation, skill invocation as a user message and quick commands; verify with line-interface tests driven by expectrl (spec: cli)
- [ ] 19.5 Implement interactive input (multiline, history, Ctrl-C semantics), themes as data, and usage display; verify with expectrl tests (spec: cli)
- [ ] 19.6 Implement resume and continue resolution (by name picks the newest numbered session, per-terminal continue, explicit cwd overrides restore), the conversation recap on resume (tool-only messages skipped) and seed prompts and prompt files (slash-leading seed text stays text, file bodies are untrusted); verify with CLI tests (spec: cli)
- [ ] 19.7 Implement composer image attachments (described for non-vision models), external editor composition (an empty file cancels), shell escape in the REPL (output kept local) and job-control suspend and resume; verify with expectrl tests (spec: cli)
- [ ] 19.8 Implement tool progress display with the focus view, display preferences (e.g. bell on approval), `chat --usage-file` written atomically on every exit path, and the flag that ignores profile config and rules; verify that the usage file is written on failure and SIGINT (spec: cli)
- [ ] 19.9 Implement local transcript and session commands (status without a model call, all-sessions views refused for non-admins), the interactive session browser with status tags, retry semantics (retry replaces the answer) and queue management commands (list, edit, reorder, drop); verify with expectrl tests (spec: cli, spec: session-storage)
- [ ] 19.10 Implement the setup wizard (provider choice incl. subscription sign-in, writes config and secrets to the right stores); verify with a scripted run on a temp home (spec: cli, spec: configuration-and-profiles)
- [ ] 19.11 Implement onboarding state (tips shown once), the tool configurator (unknown providers refused) and the plain-text setup summary (URL credentials redacted); verify with scripted runs on a temp home (spec: cli, spec: observability)
- [ ] 19.12 Implement `prometheus doctor` with the check groups, `--online`, `--json`, the enumerated `--fix` repairs and `doctor security`; verify that each repair is printed and that no network request happens without `--online` (spec: workspace-architecture, spec: cli, spec: security-boundaries)

## 20. M1 Distribution and self-update

- [ ] 20.1 Configure `cargo-dist` for every target in D11 with checksums, minisign signatures, SPDX SBOM, provenance attestation and a signed `release-manifest.json`; verify with a dry-run release in CI that every artifact is produced and verified (spec: distribution-and-update)
- [ ] 20.2 Implement the release identity grammar (incl. canary `vX.Y.Z-canary.<ts>`), the embedded trust root (primary and standby keys) and signature verification with no skip flag; verify that a tampered archive fails with `signature verification failed` (spec: distribution-and-update, spec: security-boundaries)
- [ ] 20.3 Ship shell and PowerShell installers that write the install receipt (owner, channel); verify the installers in CI containers (spec: distribution-and-update)
- [ ] 20.4 Implement `prometheus update [--check] [--channel]` with telemetry-free checks, exit 10 for an available update, the single-writer update lock, refusal for installs owned elsewhere (exit 69 naming the owner), atomic swap with a health check, update receipts and `--rollback`; verify with a local release server fixture (spec: distribution-and-update)
- [ ] 20.5 Implement schema migrations around updates (pre-migration snapshot so rollback is safe); verify that an update followed by a rollback reopens the old store (spec: distribution-and-update, spec: session-storage)
- [ ] 20.6 Implement the managed runtime store (hash-pinned index, atomic rename), the lazy installation policy, `prometheus runtimes` CLI, leases with GC and the download retry budget; verify that a digest mismatch deletes the download and fails (spec: distribution-and-update)
- [ ] 20.7 Add distribution checks to doctor and reproducible locked builds (`--locked`, a rebuild diff); verify that two CI builds of the same commit produce identical digests on Linux musl (spec: distribution-and-update)
- [ ] 20.8 Implement persistent runtime opt-outs (an opted-out runtime such as the browser is never lazily installed), download mirrors whose artifacts are verified against the signed manifest, and the dependency release-age gate in `scripts/check` (crates younger than the minimum age rejected); verify that a mirror serving a tampered archive fails and a day-old crate is rejected (spec: distribution-and-update)
- [ ] 20.9 Implement the pre-update backup (state snapshot by default, full backup on request, clear refusal when the disk is full); verify with the release-server fixture (spec: distribution-and-update, spec: configuration-and-profiles)
- [ ] 20.10 Write the install and update guide and verify the documented commands in CI (spec: distribution-and-update)

## 21. M1 Offline demo and exit criteria

- [ ] 21.1 Implement `prometheus demo [--output DIR] [--keep]` with the bundled ledger scenario and mock script, run in a temporary profile; verify that it prints canonical trajectory steps, `demo passed`, reports `Total: 85.15` and exits 0 within 30 s offline (spec: replay-and-evaluation)
- [ ] 21.2 M1 exit: in CI, on every Tier 1 target and with networking disabled, `prometheus demo` passes end to end and the Tier 2 targets pass the offline smoke run (spec: replay-and-evaluation, spec: workspace-architecture)
- [ ] 21.3 M1 exit: every M1 scenario in workspace-architecture, configuration-and-profiles, session-storage, observability, security-boundaries, execution-environments, model-providers, tool-system, agent-turn-loop, context-compression, skills, memory, cli, distribution-and-update and web-and-media-tools maps to a passing test (a scenario-to-test index checked by `scripts/check`)
- [ ] 21.4 M1 exit: the startup, memory and size gates pass on the reference runner, and `scripts/check` is green on main

## 22. M2 Daemon and client RPC protocol

- [ ] 22.1 Implement `pmt-gateway` daemon start/stop/status with one daemon per home, the discovery file and local control channel, and `daemon ensure --json`; verify that a second daemon start finds the first and exits non-zero (spec: workspace-architecture, spec: messaging-gateway)
- [ ] 22.2 Implement ticket-authenticated loopback WebSocket attachment and the shared close-code table (1000…4426); verify that a bad ticket closes with 4401 and a bad Origin with 4403 (spec: workspace-architecture)
- [ ] 22.3 Implement JSON-RPC 2.0 framing, the error-code table, `rpc.hello` handshake and versioning (major mismatch → -32008/4426); verify with protocol conformance tests (spec: workspace-architecture)
- [ ] 22.4 Generate `schema/rpc.json` and TypeScript bindings from the Rust types and add them to the freshness check; verify that changing a param type without regenerating fails CI (spec: workspace-architecture)
- [ ] 22.5 Implement the catalog index of capability-owned RPC methods and events (each name registered with its owning capability, served over the protocol and exported into `schema/rpc.json`); verify that the catalog lists every capability method and that an unregistered method name fails at startup (spec: workspace-architecture)
- [ ] 22.6 Implement strict param validation, profile and ownership checks on every session method, server-to-client requests, event sequencing with reconnect replay, liveness/backpressure, and stale-build detection; verify with tests for replay after a dropped connection (spec: workspace-architecture)
- [ ] 22.7 Implement `prometheus rpc` (debug client); verify that it can call `rpc.hello` and a session method (spec: workspace-architecture)
- [ ] 22.8 Implement CLI daemon attachment with in-process fallback and a single interactive owner per session; verify that two CLIs attaching to one session get an ownership error for the second (spec: cli)
- [ ] 22.9 Implement live config reload with apply classes, the managed configuration layer, profile alias launchers, the served profile set and profile export without credentials by default; verify with tests that a reload marked `next_session` does not alter a live session (spec: configuration-and-profiles)
- [ ] 22.10 Implement managed secrets with managed visibility (managed keys shown as managed and read-only) and profile creation options (clone config, skills or memory; messaging channels not cloned by default); verify with CLI tests (spec: configuration-and-profiles)
- [ ] 22.11 Add shell completions (`prometheus completions <shell>`); verify generated completions load in bash and zsh (spec: cli)
- [ ] 22.12 Document the client protocol (generated reference plus guide) and verify the docs build (spec: workspace-architecture)

## 23. M2 TUI

- [ ] 23.1 Implement the ratatui TUI launch selection and attachment to the daemon as a client, plus standalone in-process mode; verify with a pty test that `prometheus` on a TTY with `ui.interface = "tui"` attaches (spec: tui)
- [ ] 23.2 Implement terminal state restoration on every exit path (panic, signal) and the in-TUI update hand-off; verify with pty tests that the terminal mode is restored after SIGTERM (spec: tui)
- [ ] 23.3 Implement the TUI side of the protocol (handshake, typed method registry, error and close codes, attachment-scoped session access, sequencing and replay, liveness, non-blocking long requests, retirement, server requests with fail-closed deadlines, idempotent creation, busy submission, author stamping, profile scope); verify against the shared conformance suite (spec: tui)
- [ ] 23.4 Implement the slash command flow, discovery RPCs that preserve the prompt cache, composer with path completion, status line with accessibility, configurable key bindings and orphan attachment reaping; verify with snapshot tests (spec: tui)
- [ ] 23.5 Implement client-local display commands (status bar and indicator toggles that never reach the daemon), busy indicator styles, the fuzzy command palette (inserts, never runs) and the collapsible startup banner; verify with pty snapshot tests (spec: tui)
- [ ] 23.6 Implement large pastes collapsing into chips, clipboard image attachments (with the over-SSH fallback), LaTeX math rendering with an unsupported-macro fallback and terminal background detection without `COLORFGBG`; verify with snapshot tests (spec: tui)
- [ ] 23.7 Implement the live session switcher and auto-resume of the last TUI session after a dropped connection; verify with a pty test that reconnects after the transport is killed (spec: tui)

## 24. M2 Messaging gateway core

- [ ] 24.1 Implement profiles served by one daemon, routing identity with deterministic session keys, DM/group/thread isolation, and single-flight routing; verify with table tests of session keys (spec: messaging-gateway)
- [ ] 24.2 Implement the busy input policy from config only, command busy policy from the registry, FIFO queued turns, stop preserving the session, and conversations that never rotate on time; verify with gateway tests (spec: messaging-gateway)
- [ ] 24.3 Implement gateway slash commands, quick commands, typed lifecycle hooks and command access tiers; verify that a user without a tier gets a refusal (spec: messaging-gateway)
- [ ] 24.4 Implement the authorization order with a fail-closed default and the pairing flow (`prometheus pairing`); verify that an unpaired user is refused and a paired one is admitted (spec: messaging-gateway)
- [ ] 24.5 Implement approval requests routed to every surface, streaming to platforms, background/internal notifications and a durable delivery outbox; verify with a fake adapter (spec: messaging-gateway)
- [ ] 24.6 Implement crash recovery with a clean-shutdown marker, stuck-loop suspension, agent cache and eviction and the all-adapters-down policy; verify by killing the daemon mid-turn and checking recovery (spec: messaging-gateway)
- [ ] 24.7 Implement the relay connector, content-free opt-in health export, per-user rate limits and per-chat policies; verify with a relay fixture and rate-limit tests (spec: messaging-gateway)
- [ ] 24.8 Implement `prometheus daemon install-service` per OS without elevation (systemd user unit, LaunchAgent, Task Scheduler with a fallback; system scope requires root; idempotent re-install) and the daemon inventory across homes; verify with unit-file generation tests per OS and a two-home inventory test (spec: messaging-gateway)
- [ ] 24.9 Implement relay enrollment (single-use tokens) and the global emergency pause (persisted across restarts, in-flight turns finish, new messages get a pause notice); verify with gateway tests (spec: messaging-gateway)
- [ ] 24.10 Implement the home channel and `/sethome`, the one-shot send command `prometheus gateway send` (targets, stdin/file bodies, no agent turn) and the `send_message` tool (unknown chats refused, guard faults fail closed); verify that cron output goes home and that piping a status line delivers it (spec: messaging-gateway, spec: agent-interop)
- [ ] 24.11 Document the gateway (setup, pairing, policies) and verify commands with the docs-snippet test (spec: messaging-gateway)

## 25. M2 Platform adapters

- [ ] 25.1 Implement the adapter contract host side (capability descriptor as source of truth, degradation of optional capabilities, typed platform names, platform config shape, scoped credential access, credential locks across homes); verify with a mock adapter (spec: platform-adapters)
- [ ] 25.2 Implement inbound normalization, typed admission, dedup and text batching, fence-aware splitting with default limits, oversized output handling and send retries; verify with unit tests for splitting inside code fences (spec: platform-adapters)
- [ ] 25.3 Implement inbound media limits and caching, outbound attachments with path safety, voice transcription, voice replies/streaming TTS, typing indicator and human delay; verify with the mock platform (spec: platform-adapters)
- [ ] 25.4 Implement the shared HTTP listener for webhook platforms, adapter state reporting and reconnect, interactive approval cards and access-policy delegation; verify with integration tests (spec: platform-adapters)
- [ ] 25.5 Implement the `telegram` and `discord` adapters; verify each against the conformance kit with a mock platform server (spec: platform-adapters)
- [ ] 25.6 Implement the `slack` and `email` adapters; verify against the conformance kit (spec: platform-adapters)
- [ ] 25.7 Implement the `matrix` and `signal` adapters; verify against the conformance kit (spec: platform-adapters)
- [ ] 25.8 Implement the `whatsapp` and `webhook` adapters; verify against the conformance kit (spec: platform-adapters)
- [ ] 25.9 Ship the adapter conformance kit as `prometheus plugins test-adapter <path>`; verify that it passes for all eight first-party adapters (spec: platform-adapters)

## 26. M2 Platform adapter features and webhooks

- [ ] 26.1 Implement first-party adapter transports with required-settings validation at start (e.g. Signal daemon unreachable, email host missing), adapter proxy settings with ambient proxy env ignored, and native command menus with slash registration (priority entries survive the platform cap, followers never overwrite); verify with the conformance kit (spec: platform-adapters)
- [ ] 26.2 Implement per-platform markup conversion (tables on Telegram, links on SMS), processing lifecycle reactions, clarify multi-select on platforms, container media path translation, deliverable mode and response-wide delivery modifiers including kanban artifacts; verify with golden conversion tests (spec: platform-adapters)
- [ ] 26.3 Implement Telegram webhook mode (secret required, forged updates rejected), Telegram topics with per-topic skills and Discord auto-threading and forum channels; verify against mock platform servers (spec: platform-adapters)
- [ ] 26.4 Implement the Slack app manifest generator, Slack slash commands and the thread command prefix, email adapter behaviour (spoofed From rejected, threaded replies), Signal specifics (note to self) and WhatsApp pairing, modes and interactive features (clarify as a poll); verify against mock platform servers (spec: platform-adapters)
- [ ] 26.5 Implement webhook routes (event filters, prompt rendering), webhook limits, idempotency and responses (redelivery deduplicated), payload filters and sandboxed transform scripts confined to the scripts directory, and direct delivery mode (zero-LLM forwarding); verify with signed webhook fixtures (spec: platform-adapters)

## 27. M2 Gateway chat controls, display and routing

- [ ] 27.1 Implement platform start pings and the paginated command browser, text approval and busy-mode commands (busy mode persisted), discarding a restart-interrupted turn and the session stall watchdog with reconnect attention escalated once; verify with the fake adapter (spec: messaging-gateway)
- [ ] 27.2 Implement adapter operations from chat (e.g. clearing a breaker) and restart and update from chat with confirmation in the same thread; verify with admin-tier tests that a denied confirmation does nothing (spec: messaging-gateway)
- [ ] 27.3 Implement display policy with per-platform overrides, the runtime metadata footer kept out of model context, per-platform warning notification suppression, message timestamps in model context (new sessions only) and intentional silence tokens; verify with the fake adapter (spec: messaging-gateway)
- [ ] 27.4 Implement group mention gating with observe mode, bot-authored message admission (ignored by default, multipart collection), channel history backfill up to the bot's last reply, role-based access grants that deny on lookup failure, and per-chat policy thread matching with skill preload; verify with table tests (spec: messaging-gateway)
- [ ] 27.5 Implement multi-session DM topics (lobby redirect, restore into a new topic), the interactive model picker with a chat model override that survives `/new`, background sessions from chat, session handoff to a messaging platform and branching into a sibling thread; verify with the fake adapter (spec: messaging-gateway)
- [ ] 27.6 Implement native platform streaming and task cards with fallback, webhook route delivery with the read-only default toolset, shared-credential profile routes (target profile must opt in) and concurrency caps across surfaces (excess turns queue); verify with integration tests (spec: messaging-gateway)

## 28. M2 WASM plugin host

- [ ] 28.1 Define the WIT worlds (`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`, `context-engine`, `terminal-backend`, `ui-panel`, `delegation-router`) with versioning rules; verify that a sample component per world builds with `cargo component` (spec: plugin-system)
- [ ] 28.2 Implement `plugin.toml` manifest parsing (kinds `wasm`/`mcp`/`python`), discovery sources and identity, and opt-in enablement; verify that a discovered plugin stays disabled until enabled (spec: plugin-system)
- [ ] 28.3 Implement capability declarations and user-approved, digest-bound grants pinned to plugin identity; verify that a changed digest invalidates the grant (spec: plugin-system, spec: security-boundaries)
- [ ] 28.4 Implement the wasmtime host with fuel, epoch interruption and memory caps, host interfaces, profile-scoped plugin state and isolation; verify that an infinite loop is interrupted within the bound (spec: plugin-system)
- [ ] 28.5 Implement plugin tool registration, the typed hook registry (closed list), versioned payloads, hook isolation and time bounds, `pre_tool_call` directives and `pre_llm_call` context that preserves the prompt cache; verify that an unknown hook name fails at load (spec: plugin-system)
- [ ] 28.6 Implement bounded frozen system-prompt sections, plugin slash commands, the event bus, plugin-bundled skills, the LLM access gate and fail-closed provider plugins; verify with sample plugins (spec: plugin-system)
- [ ] 28.7 Implement load order, dependencies and deadlines, hot reload that respects the prompt-cache invariant, and safe mode (`--safe-mode`); verify that reload defers tools to the next session unless `--now` (spec: plugin-system)
- [ ] 28.8 Implement the plugin catalog, kill list, signed packages with provenance and `--allow-unsigned` recorded, and the `plugins` CLI; verify that an unsigned catalog install is refused by default (spec: plugin-system, spec: security-boundaries)
- [ ] 28.9 Write the plugin authoring guide with a working example plugin and verify the example builds and loads in CI (spec: plugin-system)

## 29. M2 Plugin hooks, shell hooks and extension surfaces

- [ ] 29.1 Implement transform hooks that rewrite tool results and replies (held replies never leak), streaming and lifecycle observer hooks (cannot change output), the bounded `pre_verify` continuation hook, `transform_api_error_classification` and approval and human-input observer hooks; verify with sample plugins (spec: plugin-system)
- [ ] 29.2 Implement shell hooks with first-use consent (exit 2 blocks a tool call, an edited script needs re-approval), the hooks CLI with a test command showing the fail-closed outcome, and gateway hook directories that stay inert until granted; verify with hook fixtures (spec: plugin-system)
- [ ] 29.3 Implement outbound webhooks (signed deliveries, a dead endpoint never stalls a turn), plugin CLI subcommands, approval transports that cannot widen scope, plugin live messages with message injection (no cross-profile injection) and platform actions (ungranted verbs return structured errors); verify with sample plugins (spec: plugin-system)
- [ ] 29.4 Implement the install-time threat scan (a reverse shell is blocked even with `--force`) and model-provider plugins that cannot silently replace a built-in provider; verify with malicious and conflicting plugin fixtures (spec: plugin-system)

## 30. M2 Toolset surfaces and schema variants

- [ ] 30.1 Implement per-surface default toolsets (cron overrides, webhook payloads that cannot execute), platform bundles (platform tools only on their platform, plugin platform bundles), the rule that disabling a bundle never strips core, and the tool configuration summary with post-setup steps; verify with surface resolution tests (spec: tool-system)
- [ ] 30.2 Implement model-dependent schema variants (variant chosen and frozen per session, handlers accept every variant); verify that a model switch does not change the frozen variant mid-session (spec: tool-system)

## 31. M2 MCP client and server

- [ ] 31.1 Implement the MCP client (`rmcp`, stdio under the sandbox and streamable HTTP) configured per profile with `prometheus mcp add|list|rm|test`; verify against a reference MCP server fixture (spec: plugin-system)
- [ ] 31.2 Implement MCP tool naming and filtering plus runtime safety (timeouts, size bounds, crash handling); verify with a misbehaving server fixture (spec: tool-system)
- [ ] 31.3 Implement MCP server mode exposing Prometheus tools and skills under grants; verify with an MCP client conformance test (spec: plugin-system)
- [ ] 31.4 Implement tool-search progressive disclosure for large MCP surfaces; verify that only the search tool is exposed until a tool is selected (spec: tool-system)
- [ ] 31.5 Implement MCP OAuth for HTTP servers (a dead refresh token parks the server; servers without dynamic client registration need a configured client), mTLS with per-profile identity headers, config watch with recycling and discovery wait, the full `mcp` CLI with transport presets, and MCP catalog entries with include/exclude lists; verify against reference server fixtures (spec: plugin-system)
- [ ] 31.6 Implement MCP sampling (only for `trust = "full"` servers, rate-limited), MCP elicitation (declined when unattended) and per-tool MCP costs with spend limits that ask at a budget crossing even for trusted servers; verify with fixtures (spec: plugin-system)
- [ ] 31.7 Implement registry toolset aliases for MCP servers (bare names, collisions), MCP resource and prompt utility tools, tool list changes that respect the frozen surface (bursts coalesced), result sanitization (smuggled instructions and reserved meta removed), media results, and tool search listing, ranking and limits; verify with misbehaving-server fixtures (spec: tool-system)

## 32. M2 Connectors

- [ ] 32.1 Implement connector availability (feature-gated, never creating identities silently), remote connector tools reached only through the tool-search bridge (absent from the tool array, slow gateway handled) and connector calls through the dispatch pipeline (unlinked account error); verify with a mock connector gateway (spec: connectors)
- [ ] 32.2 Implement multiple accounts per connector (ambiguous account refused), the `manage_connections` tool, connection flows that always require the user (links in gateway chats, never from cron), and reconnect and rename semantics that keep history; verify with tool tests (spec: connectors)
- [ ] 32.3 Implement `prometheus connectors list|accounts|connect|rename|disconnect` (disconnect needs confirmation, Ctrl-C while waiting exits 130) and document connectors; verify with CLI tests (spec: connectors)

## 33. M2 Python bridge

- [ ] 33.1 Implement the managed CPython runtime (uv-provisioned) and per-plugin pinned environments; verify a pinned environment is created offline from a cache fixture (spec: python-bridge)
- [ ] 33.2 Implement the sidecar JSON-RPC wire protocol, typed values across the wire, one profile per sidecar and the lifecycle; verify with an echo plugin (spec: python-bridge)
- [ ] 33.3 Implement the supported `PluginContext` subset, named errors for unsupported APIs, Hermes import shims, hook name/payload translation and the static compatibility audit; verify with real Hermes example plugins as fixtures (spec: python-bridge)
- [ ] 33.4 Enforce grants at the process boundary (sandbox + egress proxy), secrets/env isolation, call timeouts with stall detection, crash isolation with restart policy and resource limits; verify that a crashing plugin does not affect the agent (spec: python-bridge, spec: security-boundaries)
- [ ] 33.5 Implement model-provider plugins through the bridge (Hermes provider plugins such as Copilot work unchanged; a sidecar crash fails the call, not the agent; a profile naming a missing bridged provider fails closed), plugin CLI commands through the bridge (argparse errors map to exit 2), host-bound plugin APIs with grant checks and named side environments; verify with real Hermes plugin fixtures (spec: python-bridge, spec: model-providers)
- [ ] 33.6 Implement Python skill scripts, registration reload rules, `prometheus bridge` diagnostics and migration of Hermes plugins; verify with an end-to-end Hermes plugin test (spec: python-bridge)

## 34. M2 Security hardening for extensions

- [ ] 34.1 Implement the loopback egress proxy that resolves DNS itself and the per-session egress policy for child processes; verify that a sandboxed child can reach only allowed hosts (spec: security-boundaries)
- [ ] 34.2 Ensure gateway-owned secrets can never be delegated to tools, children, skills or plugins, and implement plugin/sidecar isolation checks; verify with grant tests (spec: security-boundaries)
- [ ] 34.3 Implement the tamper-evident security audit log (`security audit verify`) and the security posture check; verify that editing a record breaks verification (spec: security-boundaries)
- [ ] 34.4 Implement tool result provenance tagging, approval explain/dry-run and smart approvals with a denial breaker; verify with approval tests (spec: tool-system)
- [ ] 34.5 Implement the credential vault storage (encrypted at rest, recoverable interrupted key migration), `browser_vault` fills by handle (origin match, values never in results), vault codes, cards and addresses (card fills refused from webhook sessions) and the vault management CLI (no passwords on the command line); verify with vault tests (spec: security-boundaries)
- [ ] 34.6 Implement the website blocklist (shared list files), the supply-chain advisory catalog checked for MCP, plugin and skill packages, the command secret source (helper timeout fails closed), optional PII redaction and security guidance on written code (warn mode); verify with a test per scenario (spec: security-boundaries)
- [ ] 34.7 Implement the self-termination guard (killing the agent's own processes refused even under yolo, own background processes allowed) and approval suggestions mined from history; verify with approval tests (spec: tool-system)

## 35. M2 Execution environments: containers, SSH and remote

- [ ] 35.1 Implement the `container` backend (rootless Podman/Docker, `--cap-drop ALL`, `no-new-privileges`, read-only root, tmpfs), hardening that is never silently dropped, and lifecycle with orphan reaping; verify with container integration tests (spec: execution-environments)
- [ ] 35.2 Implement the `ssh` backend, reserved built-in names and the backend isolation descriptor; verify against an SSH server container (spec: execution-environments)
- [ ] 35.3 Implement remote sandboxes as `terminal-backend` plugins; verify with a sample plugin backend (spec: execution-environments)
- [ ] 35.4 Implement container backend options (network off, privileged extra args refused), remote file sync with capped sync-back, first-party backend plugins (auto mode skips providers without credentials) and sudo password handling (interactive prompt, never for subagents); verify with container and SSH fixtures (spec: execution-environments)
- [ ] 35.5 Implement quiet-wait auto-notify (second unchanged poll notifies; cron runs wait silently), retained process receipts readable after compression and scoped to their conversation, and `read_file` document extraction (PDF, Office, SQLite schema overview, scanned PDF notice); verify with fixtures (spec: execution-environments)
- [ ] 35.6 Implement snapshot triggers, caps and the per-run toggle (one checkpoint per directory per turn, large files skipped), rollback listing, diff and selective restore (hand edits preserved, preview only), the snapshot store management CLI, the working-tree diff command and project run-recipe verification (detect-only and failing-test reporting); verify with workspace fixtures (spec: execution-environments)
- [ ] 35.7 Implement throttled background notifications, subagent process handoff, file snapshots with undo, and local browser isolation; verify that `/undo` restores a file (spec: execution-environments)

## 36. M2 Browser upgrades

- [ ] 36.1 Implement `browser_cdp` and `browser_dialog`, local browser options with headed mode (refused on a headless server) and connecting to the user's running browser (disconnect keeps it open); verify against a local Chromium (spec: execution-environments)
- [ ] 36.2 Implement first-party cloud browser providers (used only when configured), routing of private URLs to a local browser, and real-profile browsing with pin and consent revocation; verify with mock provider endpoints (spec: execution-environments)

## 37. M2 LSP diagnostics

- [ ] 37.1 Implement workspace gating (no LSP for gateway sessions in the home directory), workspace trust (fresh clones untrusted until trusted explicitly), the built-in server registry, server installation through managed runtimes (package-manager policy respected) and server overrides with custom servers; verify with registry and trust tests (spec: lsp-diagnostics)
- [ ] 37.2 Implement sandboxed language servers (read-only workspace), diagnostics introduced by an edit (only new diagnostics reported, cold servers awaited) and wait modes with timeouts and broken-server retry; verify with a rust-analyzer or pyright fixture that an undefined name is reported after a patch (spec: lsp-diagnostics)
- [ ] 37.3 Implement `prometheus lsp status|list|install|install-all|…` and document LSP diagnostics; verify with CLI tests that a missing server is named (spec: lsp-diagnostics)

## 38. M2 Subagent delegation

- [ ] 38.1 Implement `delegate_task` (contract, spawn preconditions, task validation, config defaults, role from depth) with children in isolated sessions and a capability grant that can only narrow; verify that a child cannot gain a tool the parent lacks (spec: subagent-delegation)
- [ ] 38.2 Implement named subagent definitions, child model routing and fail-closed child approvals; verify with mock-provider tests (spec: subagent-delegation)
- [ ] 38.3 Implement background and synchronous dispatch, the concurrency limit with fair queueing, durable background completions and stall detection; verify that a background child's result survives a daemon restart (spec: subagent-delegation)
- [ ] 38.4 Implement control actions scoped to the caller's spawn tree, stop semantics, live progress streaming, the structured result contract, `output_schema` validation, the summary budget, fan-out budget split/roll-up and the one-shot spawn budget; verify with tree-scoped tests (spec: subagent-delegation)
- [ ] 38.5 Implement fail-closed delegation routers, delegation settings changes that respect caching, profile routes into the target `ProfileScope`, the plugin lifecycle interface and child reaping; verify with tests (spec: subagent-delegation)
- [ ] 38.6 Implement programmatic tool calling (`execute_code`) under the caller's grant with its RPC channel, limits and pre-execution guards; verify that an empty grant intersection is refused (spec: subagent-delegation)

## 39. M2 Git worktrees

- [ ] 39.1 Implement worktree creation (post-checkout hooks run under the sandbox, generated names for unnamed trees), `chat -w` worktree mode, exit disposition (uncommitted work kept on Ctrl-D, gateway sessions handled) and the in-session worktree command that preserves the prompt cache; verify with git fixture repositories (spec: git-worktrees)
- [ ] 39.2 Implement the repository registry and sweep schedule, the sweep decision table (live processes and large untracked files protect a tree), the archive format (consistent under concurrent index changes) and the worktree CLI with restore; verify that an archived lane restores (spec: git-worktrees)
- [ ] 39.3 Implement subagent worktree isolation (child commits land on the child's branch; container backends handled); verify with a mock-provider delegation that commits in a child worktree (spec: git-worktrees)

## 40. M2 Delegation upgrades

- [ ] 40.1 Implement user-launched background sessions, the agents monitor with live transcripts (stop one child, tail a transcript), the review command and independent completions (ungrouped results arrive separately); verify with mock-provider tests (spec: subagent-delegation)
- [ ] 40.2 Implement child request overrides and direct endpoints, gateway delegation mode with one-shot joining (one message per turn on messaging platforms) and worktree isolation for children (clean children pruned, failed probes keep work); verify with mock-provider tests (spec: subagent-delegation)
- [ ] 40.3 Implement the persistent `execute_code` session kernel (state kept between calls, reset discards it) and `execute_code` execution modes (broken venv falls back); verify with kernel tests (spec: subagent-delegation)

## 41. M2 Scheduled jobs

- [ ] 41.1 Implement the schedule grammar (cron, interval, one-shot) with canonical echo and confirmation, payload validation, job records and state literals; verify with grammar table tests (spec: scheduled-jobs)
- [ ] 41.2 Implement next-run computation, grace and missed occurrences, and at-most-once firing; verify with a fake clock across a daemon restart (spec: scheduled-jobs)
- [ ] 41.3 Implement the built-in trigger in the daemon and the headless tick, execution concurrency and timeouts, fresh isolated sessions, the unattended memory policy and fail-closed approvals; verify with integration tests (spec: scheduled-jobs)
- [ ] 41.4 Implement script-backed and no-agent jobs, the prompt safety scan, pre-dispatch preflight, delivery targets via the durable outbox, and response framing and silence; verify with a fake adapter (spec: scheduled-jobs)
- [ ] 41.5 Implement run history and replay, lifecycle ops, repeat limits and retention, the pluggable trigger provider, the `cron` CLI and model tools, and the cron doctor; verify with CLI tests and document cron (spec: scheduled-jobs)

## 42. M2 Scheduled job upgrades

- [ ] 42.1 Implement the per-job working directory (context files loaded, relative paths refused), model pinning with the cron fleet default (the agent cannot choose a model), creating jobs paused with an auditable reason, the complete cronjob tool field set (timezone pin) and the `/cron` slash command; verify with job tests (spec: scheduled-jobs)
- [ ] 42.2 Implement the failure streak with a review nudge, automatic re-runs when the model was unreachable, holding through a closed provider usage window, failure incidents with alert-once and acknowledge, failure notice routing and agent-declared run failure; verify with a fake clock and fault-injected providers (spec: scheduled-jobs)
- [ ] 42.3 Implement the Bot Chat delivery target (`bot_chat[:<profile>]`, unknown profile rejected at create), the push notification flag, cron threads and delivery confirmation, and continuable deliveries (a reply continues the run's session); verify with the fake adapter (spec: scheduled-jobs)
- [ ] 42.4 Implement the script wake gate with script credentials (unchanged polls skip the model, provider keys never reach scripts), monitor mode, job chaining with continuity, the per-job notepad and asynchronous tool-initiated runs with per-run context; verify with job tests (spec: scheduled-jobs)
- [ ] 42.5 Implement the suggested automations inbox (dismissals remembered), the automation blueprint catalog (slot validation, inline values) and webhook-triggered job runs (bursts never double-fire); verify with fixtures and document the additions (spec: scheduled-jobs)

## 43. M2 Session automation: /goal, /loop and /heartbeat

- [ ] 43.1 Implement persisted per-session automation state (survives resume, cleared on reset), automated turns as plain user turns that preserve the cache prefix, idle-boundary arbitration (user input preempts), automation configuration validation and the separation from kanban and cron; verify with mock-provider sessions (spec: session-automation)
- [ ] 43.2 Implement the `/goal` command surface (control-word escape, drafts), goal changes while a turn runs, the judge loop (continue injects the next turn, a broken judge fails open) and the goal turn budget with the blocked verdict; verify with a scripted mock judge (spec: session-automation)
- [ ] 43.3 Implement completion contracts (inline parsing, draft fallback), subgoals, quality gates (a red gate blocks completion, an exhausted gate pauses) and goal wait barriers (park on a watcher, dead pids rejected); verify with mock-provider tests (spec: session-automation)
- [ ] 43.4 Implement `/loop` with cadence (interval floor, self-paced backoff) and stop conditions, the session `/heartbeat` (missed ticks coalesce), heartbeat ownership, recovery and silence on the gateway, goal messages on messaging platforms and automation status on every surface; verify with a fake clock across a daemon restart and document session automation (spec: session-automation)

## 44. M2 Turn loop, compression, memory and storage upgrades

- [ ] 44.1 Implement unpriced-model handling under a cost budget, the budget checkpoint notice and structured steering; verify with the spec scenarios (spec: agent-turn-loop)
- [ ] 44.2 Implement the deterministic turn journal and crash resume mid-tool-batch (re-run only `effect = none` or idempotent calls, otherwise "outcome unknown"); verify by killing the process mid-batch and resuming (spec: agent-turn-loop)
- [ ] 44.3 Implement install-tree context suppression and subdirectory hints that ride tool results; verify with fixtures (spec: agent-turn-loop)
- [ ] 44.4 Implement host validation of engine output, per-model threshold overrides, gateway session hygiene, retrievable pruned content, the summary quality check, pinned messages, micro-compaction, prepared-ahead summaries and proactive prune; verify with golden transcripts (spec: context-compression)
- [ ] 44.5 Implement `/btw` side questions (answered outside the main transcript, busy-safe), `/init` project instructions bootstrap (existing files preserved), prefill messages frozen at session start, verify-on-stop (unverified code edits nudged, docs-only edits pass) and the coding posture with standing coding instructions (messaging sessions unaffected); verify with mock-provider tests (spec: agent-turn-loop)
- [ ] 44.6 Implement image budgets and replay (embed budget, per-image cap including inside subagents, recent images replayed on resume); verify with mock vision routes (spec: agent-turn-loop, spec: web-and-media-tools)
- [ ] 44.7 Implement idle compaction (after a long pause, small contexts skipped), compaction progress notices with the context pressure gauge (silent on chat by default), opt-in server-side Responses compaction (compaction items replayed) and the context window view; verify with golden transcripts (spec: context-compression)
- [ ] 44.8 Implement the memory revision journal, reviewable changes per session and semantic recall; verify that a change can be reverted (spec: memory)
- [ ] 44.9 Implement the external memory provider contract (one provider, isolated and bounded, gated tools and prompt block, egress redaction and mirroring, profile-bound, pre-compress checkpoint, loud degradation, setup writing to the right stores) and context-engine plugins; verify with a sample WASM provider (spec: memory)
- [ ] 44.10 Implement the learning journey timeline (JSON nodes) with pruning and editing, per-turn memory notifications (staged changes not announced), background review controls (input budget, extra tools must exist on the parent) and the external provider recall budget with spill to file; verify with memory tests (spec: memory)
- [ ] 44.11 Implement the session forking API and portable session export/import; verify a round trip between two homes (spec: session-storage)
- [ ] 44.12 Implement offline recovery with journal-mode control (refused while the store is open), prompt refresh and retitling (user titles kept), the low-disk prune prompt ("later" postpones a week) and the attachment storage location; verify with storage tests (spec: session-storage)

## 45. M2 Additional subscription providers and routing

- [ ] 45.1 Implement the model retirement table and `prometheus config` retired-model rewrite (dry run by default); verify that a retired default model is rewritten only with confirmation (spec: model-providers, spec: configuration-and-profiles)
- [ ] 45.2 Implement provider registry changes at session boundaries, minted short-lived credentials, and OpenRouter OAuth key minting; verify with mock OAuth servers (spec: model-providers)
- [ ] 45.3 Implement xAI SuperGrok, Nous Portal, Qwen Portal and MiniMax portal sign-ins; verify each with mock IdP fixtures (spec: model-providers)
- [ ] 45.4 Implement plugin OAuth (host-run PKCE, opaque credential handle) and `external_process` providers; verify with a sample provider plugin (spec: model-providers, spec: plugin-system)
- [ ] 45.5 Implement plan and quota visibility, billing-source transparency, scoped fallback chains for pinned owners, provider health scoring (`providers health`) and provider spend caps; verify with tests (spec: model-providers)

## 46. M2 Provider adapters, plans and request controls

- [ ] 46.1 Implement endpoint auto-detection for multi-region key providers (probe results cached), session affinity headers with per-model wire routing, OpenRouter provider routing preferences, the fast mode service tier and the outbound IPv4 preference; verify with recorded HTTP fixtures (spec: model-providers)
- [ ] 46.2 Implement the Gemini native and AWS Bedrock providers (inference profiles, missing AWS credentials reported), the Azure AI Foundry provider (Entra ID keyless access, Anthropic wire for Claude deployments) and data-use confirmation for training-flagged models (refused non-interactively); verify with recorded fixtures (spec: model-providers)
- [ ] 46.3 Implement credential pool maintenance commands (status is a peek, reset reaches running sessions), pool seeding with custom endpoint pools and child sharing, external agent CLI executable resolution under a bare service PATH, auxiliary concurrency and request overrides, and WSL2 local endpoint diagnostics; verify with tests (spec: model-providers)
- [ ] 46.4 Implement the account usage windows command (`--json`), subscription account commands (login refused in group chats) and subscription tool gateway routing (stored selection wins over a present key); verify with mock subscription endpoints (spec: model-providers)

## 47. M2 Mixture of agents

- [ ] 47.1 Implement the MoA virtual provider with presets selectable as models (bare names, unknown presets refused) and preset validation (no recursive aggregator, temperature omitted by default); verify with resolution tests (spec: mixture-of-agents)
- [ ] 47.2 Implement the MoA call procedure (references never see tools, the aggregator's tool calls execute), cache-safe guidance injection, strict-alternation recovery, advisor cadence and non-fatal reference failures; verify with mock-provider scripts that the session prefix is unchanged across iterations (spec: mixture-of-agents)
- [ ] 47.3 Implement the privacy filter for advisor outputs (display-only redaction), MoA traces and per-slot billing rows in the ledger; verify with ledger and trace tests (spec: mixture-of-agents)
- [ ] 47.4 Implement MoA as a fallback entry, the one-shot `/moa` command (route restored after one turn), preset management commands (default preset protected, presets in every picker) and prompt-cache preservation when returning from MoA; verify with mock-provider tests and document MoA (spec: mixture-of-agents)

## 48. M2 Local models

- [ ] 48.1 Implement local runtime configuration (`[local_runtime]` in `daemon.toml`, detection-only default), hash-pinned llama.cpp engine installation (corrupt archives rejected, unsupported backends refused) and backend auto-selection by hardware; verify with installer fixtures (spec: local-models)
- [ ] 48.2 Implement the hardware-priced model catalog, resumable verifiable downloads (an interrupted download is unusable until verified) and the machine-wide model library (linked files never deleted); verify with a range-request fixture server (spec: local-models)
- [ ] 48.3 Implement the managed server lifecycle (shared across processes, idle unload), external llama-server detection (a user-run server preferred), typed local runtime errors (never silently falling back to a remote key) and the rule that local inference stays local; verify offline with a stub server (spec: local-models)
- [ ] 48.4 Implement `prometheus local status|install|update|catalog|pull|pause|resume|jobs|search|add|rm|use` (Ctrl-C pauses a pull) and document local models; verify with CLI tests (spec: local-models)

## 49. M2 Web and media tools

- [ ] 49.1 Implement the built-in web backends with capability flags, explicit opt-in keyless web search rotation (one-shot rescue only when opted in), web result caching shared across subagents and the `x_search` tool (future windows refused, uncited answers degraded); verify with mock backends (spec: web-and-media-tools)
- [ ] 49.2 Implement `video_analyze` (oversized videos refused or sampled), `image_generate` with the built-in image generation providers and models (editing advertised only when supported, results as media) and explicit media tool credentials and billing; verify with mock provider fixtures (spec: web-and-media-tools)
- [ ] 49.3 Implement the `text_to_speech` tool with delivery (chunking, voice bubbles), the built-in text-to-speech providers, built-in speech-to-text providers with resolution (explicit provider strict, local first when unset) and command-template speech providers; verify with mock speech providers (spec: web-and-media-tools)

## 50. M2 Voice mode

- [ ] 50.1 Implement voice mode toggle and commands (refused without a microphone, status lists providers), push-to-talk recording with the continuous loop, two-stage silence detection and spoken and typed stop phrases; verify with recorded audio fixtures and a mock STT provider (spec: voice-mode)
- [ ] 50.2 Implement the transcription hallucination filter, long recordings split under provider upload limits, sentence-streamed spoken replies (code not read aloud) and the gateway per-chat voice reply command; verify with mock STT/TTS providers and the fake adapter (spec: voice-mode)

## 51. M2 Replay and model-call cache

- [ ] 51.1 Implement the canonical request hash (excluding transport, streaming and cache-control fields) and the content-addressed model-call cache with admin commands and limits; verify that equivalent requests hash identically (spec: replay-and-evaluation)
- [ ] 51.2 Implement session recordings, replay modes, divergence detection and side-effect-free replay; verify that replaying a recorded session offline reproduces the trajectory (spec: replay-and-evaluation)
- [ ] 51.3 Add the golden-recording regression gate and the built-in smoke suite to `scripts/check`; verify that a behavior change in the loop fails the gate (spec: replay-and-evaluation)

## 52. M2 Observability upgrades

- [ ] 52.1 Implement opt-in OpenTelemetry export, the span model, trace context propagation and the metrics catalog with an optional local scrape endpoint; verify against an OTLP collector container (spec: observability)
- [ ] 52.2 Implement live usage events, health endpoints (`/healthz`, `/readyz`), the component health model and `prometheus status [--check]`; verify with daemon tests (spec: observability)
- [ ] 52.3 Implement the crash/debug bundle (redacted) and the observability overhead budget gate; verify that the bundle contains no secrets (spec: observability)
- [ ] 52.4 Implement the usage insights report (CLI and slash command, per-platform filters), configuration-centric status, export posture status (content capture visible) and the runtime liveness watchdog with a task dump; verify with daemon tests (spec: observability)
- [ ] 52.5 Implement the opt-in debug share upload (declined uploads send nothing, unattended shares refused) and ShareGPT trajectory export; verify with a mock upload endpoint and golden exports (spec: observability)

## 53. M2 Skills hub and curator

- [ ] 53.1 Implement sandboxed skill scripts with managed runtimes and the hub install pipeline with trust policy; verify with a local hub fixture (spec: skills)
- [ ] 53.2 Implement the skills lockfile with tamper detection, signatures and provenance, and blueprint skills that never auto-schedule; verify that a modified installed skill is detected (spec: skills)
- [ ] 53.3 Implement the curator (configuration, scheduling, transitions, scope, backups, CLI); verify with curator tests (spec: skills)
- [ ] 53.4 Implement hub discovery, inspection, updates and audit (local edits survive updates, inspect installs nothing), hub source identifiers, custom skill taps and skill publishing with snapshots; verify with a local hub fixture (spec: skills)
- [ ] 53.5 Implement the `/curator` slash command, the curator audit ledger with single-edit rollback, purge and reports (purge never automatic), interactive per-surface skill enablement, skill credential file passthrough (read-only mounts only with a grant) and the bundled and official skill catalog content (browsable offline, Hermes name aliases); verify with curator and catalog tests (spec: skills)

## 54. M2 Distribution channels

- [ ] 54.1 Publish package-manager and image channels (Homebrew tap, WinGet, container image, Nix flake) with receipts naming the owner; verify that self-update is refused for each with exit 69 (spec: distribution-and-update)
- [ ] 54.2 Implement running-daemon handover on update, update dry-run with changelog preview, offline bundles and `prometheus uninstall`; verify with a local release server (spec: distribution-and-update)
- [ ] 54.3 Implement the NixOS and Home Manager modules (Nix-owned keys read-only, bad config does not restart-loop); verify with a NixOS VM test (spec: distribution-and-update)
- [ ] 54.4 Implement the container image run modes, filesystem, init and health check, multiple profiles in one container, the audio bridge and host services, and the network-isolated deployment recipe; verify with container tests that zombies are reaped and isolation is verified (spec: distribution-and-update)
- [ ] 54.5 Implement update from chat (disabled by default, admin only) and uninstall scopes with dry run; verify that a dry run lists the service unit and a data wipe on a package-manager install is handled (spec: distribution-and-update)

## 55. M2 Hermes migration

- [ ] 55.1 Implement `prometheus migrate from-hermes` surface, the read-only source guarantee, profile discovery/mapping, dry run and mapping report; verify against a fixture Hermes home that the source tree hash is unchanged (spec: hermes-migration)
- [ ] 55.2 Implement config translation from user-set values only (supported versions, fail-closed ambiguous YAML, env references), `.env` re-sorting into secrets and config, credential files and typed-name validation; verify with fixture configs (spec: hermes-migration)
- [ ] 55.3 Implement streaming verified session import with lineage and usage events, memory and identity import, and skills import with scanning; verify counts and FTS search after import (spec: hermes-migration)
- [ ] 55.4 Implement cron jobs imported paused, MCP servers and plugins (bridge), gateway settings and pairing allowlists; verify with fixtures (spec: hermes-migration)
- [ ] 55.5 Implement idempotent incremental re-runs, per-component atomicity and resumability, exit codes and confirmation; verify that a second run imports nothing new and an interrupted run resumes (spec: hermes-migration)
- [ ] 55.6 Implement importers for other agents (detection, broken settings reported), the rule that other agents never yield secrets by default, the Claude Code, Codex CLI and Cursor mappings and the OpenClaw mapping; verify with fixture homes for each agent (spec: hermes-migration)
- [ ] 55.7 Implement source archiving after import (explicit cleanup only), keeping imports in sync (token refreshes do not trigger syncs) and session transcripts from other agent CLIs (idempotent re-import); verify with fixture homes (spec: hermes-migration)
- [ ] 55.8 Write the migration guide and verify its commands against the fixture home (spec: hermes-migration)

## 56. M2 Projects

- [ ] 56.1 Implement project records (one owner per folder), project commands (create, activate, list, show), the active project with path ownership (nested folders resolved) and archive and restore (an archived active project is cleared); verify with CLI tests (spec: projects)
- [ ] 56.2 Include projects in backups and exports (missing folders flagged on restore); verify with a backup round trip (spec: projects)

## 57. M2 Internationalization

- [ ] 57.1 Implement static UI string catalogs (language changes keep the prompt cache), the bundled languages, `display.language` selection per profile (unknown ids refused) and layered lookup with partial catalogs; verify with catalog tests (spec: i18n)
- [ ] 57.2 Implement placeholder and value validation (misspelled placeholders fall back), language-pack plugins with pack validation (unknown keys warned), language changes applied without restart and migration of Hermes language settings; verify with a no-code pack fixture (spec: i18n)

## 58. M2 CLI upgrades

- [ ] 58.1 Implement streaming JSONL output (ends with a result record), the planning-only turn (writes outside plans refused) and exit-with-delete and profile archive slash forms; verify with CLI tests (spec: cli)
- [ ] 58.2 Implement theme commands with branding keys, setup wizard modes (quick setup asks only unset values, adapter fields from the schema) and the Windows console behaviour (code page restored); verify with snapshot tests (spec: cli)
- [ ] 58.3 Implement the safe command console (shell metacharacters refused), webhook subscriptions (secret printed once) and background sessions with results delivered back; verify with CLI tests (spec: cli)

## 59. M2 OpenAI-compatible API server

- [ ] 59.1 Implement the opt-in API listener on `8642` that fails closed without a per-profile key (public binds need explicit opt-in), profile-bound routing with bearer authentication, and browser access rules with Host checks and security headers; verify with HTTP security tests (spec: api-server)
- [ ] 59.2 Implement model listing with Open WebUI compatibility, the Chat Completions endpoint (budget exhaustion is not success, uploaded files refused), API session identity and ownership, and caller instructions with long-term memory scoping; verify with an OpenAI SDK client test (spec: api-server)
- [ ] 59.3 Implement the Responses endpoint with its response store (chained turns, echoed reasoning items ignored) and idempotent admission for Chat Completions and Responses (key reuse for different work refused); verify that a retried request after network loss runs once (spec: api-server)
- [ ] 59.4 Implement the Runs API lifecycle, the run idempotency ledger (replay after restart), the run events stream (reconnect without loss), run controls (stale approval generations refused), unattended approvals with the API toolset, per-request model selection and the concurrent run cap; verify with integration tests (spec: api-server)
- [ ] 59.5 Implement the Sessions REST API (fork and continue, light transcripts), the Jobs REST API and discovery and readiness endpoints that hide secrets; verify with HTTP tests and document the API server (spec: api-server)

## 60. M2 Exit criteria

- [ ] 60.1 M2 exit: an end-to-end test starts the daemon, attaches the TUI and a CLI client, sends a message through the mock platform adapter, receives a streamed reply, and resumes after a dropped connection with no lost events (spec: workspace-architecture, spec: tui, spec: messaging-gateway)
- [ ] 60.2 M2 exit: a WASM plugin, an MCP server and a Hermes Python plugin each register a tool that the agent calls under grants, and an ungranted capability is refused in each (spec: plugin-system, spec: python-bridge)
- [ ] 60.3 M2 exit: a cron job delegating to a background subagent fires exactly once across a daemon restart and delivers via the outbox (spec: scheduled-jobs, spec: subagent-delegation)
- [ ] 60.4 M2 exit: an OpenAI SDK client completes a chat and an idempotent retried run through the API server, a `/goal` session reaches completion under the mock judge, a MoA preset answers through the mock provider, and a delegated child commits in its own git worktree (spec: api-server, spec: session-automation, spec: mixture-of-agents, spec: git-worktrees)
- [ ] 60.5 M2 exit: voice mode transcribes a recorded fixture and speaks the reply through mock STT/TTS, an LSP diagnostic is reported after an edit, a connector tool is reached through tool search, a stub local model answers offline, a project is created and activated, `gateway send` delivers to a home channel, and the TUI renders in a bundled non-English language (spec: voice-mode, spec: web-and-media-tools, spec: lsp-diagnostics, spec: connectors, spec: local-models, spec: projects, spec: agent-interop, spec: i18n)
- [ ] 60.6 M2 exit: migrating the fixture Hermes home and running the golden replay suite pass in CI; every M2 scenario across specs maps to a passing test in the scenario index

## 61. M3 Web dashboard

- [ ] 61.1 Implement `prometheus dashboard` on `127.0.0.1:9120` with the SPA embedded in the binary, listener lifecycle within the daemon and a configurable reverse-proxy base path; verify that the binary serves the SPA offline (spec: web-dashboard)
- [ ] 61.2 Implement loopback auth with an HttpOnly session cookie, CSRF protection, fail-closed non-loopback binds, Host validation, same-origin only and the public endpoint list; verify with HTTP security tests (spec: web-dashboard)
- [ ] 61.3 Implement profile-addressed routes, structured chat over the client protocol, observer-by-default remote users and the live event stream; verify with Playwright E2E tests (spec: web-dashboard)
- [ ] 61.4 Implement the built-in pages, write-only secrets, schema-driven config editing, the honest platform matrix, redacted bounded logs, upload/download limits, and the cost, harness-evolution and experience views; verify with Playwright tests (spec: web-dashboard)
- [ ] 61.5 Implement the REST endpoint inventory (config schema served from the typed config, bulk maintenance needs confirmation), additional dashboard sign-in providers (token mode refused without its secret) and registering a dashboard with the account portal; verify with HTTP tests (spec: web-dashboard)
- [ ] 61.6 Implement paired device credentials for remote clients (revocation, expiring pairing codes), remote desktop clients attaching through the dashboard listener, the gateway's remote client attachment (off by default, revoked devices refused), the desktop workspace in the browser, webapp sign-out across tabs with the insecure-context notice, and webapp file staging and terminals; verify with Playwright tests (spec: web-dashboard, spec: messaging-gateway)
- [ ] 61.7 Implement the kanban dashboard API with orchestration modes, the webhooks page (secret shown once), channel setup forms, the files page (symlink escapes refused), embedded documentation (offline), the profile builder (clones without secrets), dashboard themes (no remote CSS, size cap) and dashboard plugin contributions (no cookie access from plugin frames); verify with Playwright tests (spec: web-dashboard)

## 62. M3 Desktop app

- [ ] 62.1 Scaffold the Tauri 2 app in `apps/desktop` reusing the dashboard components, with daemon attachment through the bundled binary (`daemon ensure` bounded), local endpoint validation and version-skew restart; verify with a desktop E2E test (spec: desktop-app)
- [ ] 62.2 Implement one-use dial tickets kept out of webview storage, shared client defaults and reconnect with full-jitter backoff; verify that tickets never appear in webview storage (spec: desktop-app)
- [ ] 62.3 Implement the native command allowlist, windows that never open themselves, the external URL scheme allowlist and deep links; verify with Tauri tests (spec: desktop-app)
- [ ] 62.4 Implement desktop-sourced sessions, the gone-session verdict, the slash palette from the registry, atomic bot chats, validated settings patches and explicit profile switching; verify with desktop E2E tests (spec: desktop-app)
- [ ] 62.5 Implement background running after windows close, approval notifications that never approve, isolated plugin UI panels and the global hotkey quick-ask; verify that a notification click opens the card without approving (spec: desktop-app)
- [ ] 62.6 Implement install deep links that open a consent flow (arbitrary URLs refused), launching the desktop from the CLI (verified download, project directory), desktop launch options and machine-wide UI preferences shared with the webapp; verify with desktop E2E tests (spec: desktop-app)
- [ ] 62.7 Ship desktop packages with signing per OS (store installs refuse self-update, CLI and desktop cannot skew); verify signed artifacts in the release dry run (spec: distribution-and-update, spec: desktop-app)
- [ ] 62.8 Implement signed in-app updates on `stable`/`canary` and no updater in dev builds, plus honest platform support in settings; verify that a tampered update is rejected (spec: desktop-app, spec: distribution-and-update)

## 63. M3 Desktop chat, composer and workspace panes

- [ ] 63.1 Implement chat transcript rendering (timeline jumps, remote images gated), durable drafts that are never sent for the user, composer history with queue editing (a lost turn blocks the queue), the desktop status bar (context inspection, session-scoped bypass), the composer model picker (does not change the default) and subagent route settings as drafts until applied; verify with desktop E2E tests (spec: desktop-app)
- [ ] 63.2 Implement the file browser pane (container sessions, large-file previews), the artifacts gallery with suggestions, the preview rail with the in-app browser (no native commands from pages) and annotate mode (password fields redacted, pins never sent); verify with desktop E2E tests (spec: desktop-app)
- [ ] 63.3 Implement windows, tabs and panes (pop-out sessions), interface modes and layouts, embedded terminals as daemon-owned PTYs (cap enforced, observers cannot type) and the live subagents frame (steering acknowledged as queued); verify with desktop E2E tests (spec: desktop-app)
- [ ] 63.4 Implement the git review pane (last-turn scope, revert needs confirmation), the memory graph (content-free share codes), HUD mode, management panes (cross-profile actions scoped), the messaging pane showing saved versus running state, Telegram quick setup by QR, repository discovery for the Projects sidebar (excluded paths skipped) and fonts, shortcuts, palette and profile files (exports without keys); verify with desktop E2E tests (spec: desktop-app, spec: projects)

## 64. M3 Desktop remote connections

- [ ] 64.1 Implement the connection registry (duplicate URLs refused, listed to plugins), remote connections that are authenticated and encrypted (plain HTTP to LAN hosts refused, certificate changes stop the connection) and SSH connections reusing the local ticket model (host key changes, missing remote binary); verify with a remote daemon fixture (spec: desktop-app)
- [ ] 64.2 Implement gateway switching and scoping (approvals from background connections, failed switches not remembered), updating every connected instance (unreachable instances reported) and agents across connections (same profile names disambiguated, mentions never cross machines); verify with two daemon fixtures (spec: desktop-app)

## 65. M3 Desktop UI toolset

- [ ] 65.1 Implement the desktop UI toolset contract (absent without an attached window, tips toggle mid-session), agent reactions (own messages only), terminal pane tools and the preview pane tool (private addresses blocked); verify with desktop E2E tests (spec: desktop-app)
- [ ] 65.2 Implement driving the preview page (stale refs, password typing gated), annotating the preview page (navigation clears marks) and reading the window below (permission prompts on macOS); verify with desktop E2E tests (spec: desktop-app)
- [ ] 65.3 Implement pane focus, tours and tips, the appearance tool, the project tool for GUI sessions (switching keeps the prompt cache) and the first-run onboarding chat (tool only in that chat, card deadline); verify with desktop E2E tests (spec: desktop-app, spec: projects)

## 66. M3 ACP adapter

- [ ] 66.1 Implement `prometheus acp` launch flags, stdout-only protocol frames, quiet liveness probes and config from flags/profile only, as a daemon viewer bound to the launch profile; verify with an ACP client test that stdout carries only frames (spec: acp-adapter)
- [ ] 66.2 Implement initialize, auth reuse, new/load/resume/list/fork sessions and editor MCP servers owned by the daemon session; verify with ACP conformance fixtures (spec: acp-adapter)
- [ ] 66.3 Implement prompt submission with exactly-once submit, scoped cancel, unknown outcome after a daemon restart, slash commands, edit-approval modes, validated config options, model switching, fail-closed approval/clarify bridging, exactly-once reply projection and terminal tool statuses; verify with fault-injection tests (spec: acp-adapter)
- [ ] 66.4 Implement content conversion with media staging, metadata projection, editor-native file access and terminal delegation; verify with an editor fixture (spec: acp-adapter)
- [ ] 66.5 Implement optional browser tools for ACP sessions (runtime missing reported) and unattended ACP clients with relay bridges (auto-approval only through the declared bridge, unattended default toolset); verify with ACP fixtures (spec: acp-adapter)

## 67. M3 Kanban

- [ ] 67.1 Implement board storage at `<home>/kanban/`, slugs and selection, the typed workflow table, task creation (idempotent) and dependency links; verify with store tests (spec: kanban)
- [ ] 67.2 Implement ready promotion with atomic claim, typed blocks with the loop breaker, scheduled parking, completion gates, the review lane and archive terminating the worker; verify with concurrency tests (spec: kanban)
- [ ] 67.3 Implement the single dispatcher per home, its tick, concurrency caps, the fail-closed claim allowlist, the failure circuit breaker, worker spawn by run ticket and liveness heartbeats; verify that a dead worker's task is reclaimed (spec: kanban)
- [ ] 67.4 Implement the kanban toolset and grants, workspaces, attachments, notifications through the owning profile, per-task cost budget, eval/evolution tasks, event log and run history, the CLI and board export/import; verify with CLI tests and document kanban (spec: kanban)

## 68. M3 Kanban upgrades

- [ ] 68.1 Implement URL attachments with attachment listing tools (internal addresses refused), PR completion contracts (red required checks block completion, retries cannot substitute a sibling PR), goal-mode cards, per-task model overrides with pinned skills and the respawn guard with dispatch dry run; verify with board tests (spec: kanban)
- [ ] 68.2 Implement the swarm topology helper (atomic creation, unknown worker profile aborts everything), the `/kanban` slash command, subscription delivery modes with inheritance, orchestrator guidance with specify and decompose, and the additional CLI verbs; verify with CLI tests (spec: kanban)
- [ ] 68.3 Implement worker lanes (external CLI lanes declared under `[[kanban_lanes]]`, unknown assignees surfaced) and kanban lifecycle observer hooks that cannot hold the board; verify with a sandboxed lane fixture (spec: kanban, spec: plugin-system)
- [ ] 68.4 Implement project kanban board binding (branch convention); verify that a project's board is selected by default (spec: projects)

## 69. M3 Harness evolution

- [ ] 69.1 Implement opt-in gating, the harness tree layout, editable surface with component numbering, size caps, validation that collects every error (exit 65) and loading once per session; verify `prometheus harness validate` on fixtures (spec: harness-evolution)
- [ ] 69.2 Implement execution confinement, history/diff (`harness log|show|diff`), rollback and manual edits; verify that a rollback writes a new version (spec: harness-evolution)
- [ ] 69.3 Implement task capture, on-demand/scheduled/automatic triggers, modes with budget k → R=4k rollouts and P=3k improver sessions, and labels; verify the budget arithmetic with tests (spec: harness-evolution)
- [ ] 69.4 Implement parallel and sequential modes with isolated rollouts, side-effect recorders for `effect = external` tools, improver sessions, stage acceptance/retry/fallback and the evidence context; verify with the mock provider (spec: harness-evolution)
- [ ] 69.5 Implement answer-free prompts, leak globs with the verdict tripwire, the budget ledger and spend cap, and concurrency limits; verify that a planted rubric file aborts the job (spec: harness-evolution)
- [ ] 69.6 Implement blind anonymised selection, user-driven selection, proposals with adoption policy (approval required for scripts, hooks and subagents; never widening capabilities), stale-proposal protection, job lifecycle/events/RPC, resume with the content-addressed session cache and human-only evaluation; verify with end-to-end mock jobs (spec: harness-evolution)

## 70. M3 Experience library

- [ ] 70.1 Implement the library layout, domain names and resolution, the reflection workspace and distillation; verify with a mock-provider job (spec: experience-library)
- [ ] 70.2 Implement write-on-adoption only, entry contents, answer-free and secret-free entries, the index, exposure to proposers and stream order; verify that a rejected proposal writes no entry (spec: experience-library)
- [ ] 70.3 Implement lessons offered to later sessions, rollback demotion, the `experience` CLI, the retention cap, export/import between profiles, restore on resume and repair, and library events; verify with CLI tests (spec: experience-library)

## 71. M3 Evaluation suites

- [ ] 71.1 Implement the eval suite format and `prometheus eval run` with resumable runs; verify a suite run against the mock provider (spec: replay-and-evaluation)
- [ ] 71.2 Implement the answer-free boundary, post-hoc judging, re-judging with judge identity, and reports with comparison; verify that no grader data reaches the agent's context (spec: replay-and-evaluation)
- [ ] 71.3 Implement the benchmark adapter protocol and session-to-task capture; verify with a sample adapter (spec: replay-and-evaluation)
- [ ] 71.4 Implement batch prompt datasets (resumable, ephemeral prompts not recorded), batch statistics with quality filtering, toolset distributions (reproducible sampling) and the trajectory compressor (head and tail preserved); verify with dataset fixtures (spec: replay-and-evaluation)
- [ ] 71.5 Implement the offline self-improvement demo; verify that it runs a full evolution job with the mock provider offline and exits 0 (spec: replay-and-evaluation, spec: harness-evolution)

## 72. M3 Remaining upgrades

- [ ] 72.1 Implement memory decay/expiry/dormancy and consolidation proposals; verify with fake-clock tests (spec: memory)
- [ ] 72.2 Implement hybrid semantic + full-text session search and the encryption-at-rest option; verify search ranking fixtures and that an encrypted db is unreadable without the key (spec: session-storage)
- [ ] 72.3 Implement TUI split panes, inline diff review, the timeline scrubber and shared live sessions; verify with pty snapshot tests (spec: tui)
- [ ] 72.4 Implement skill evals and eval-gated skill edits; verify that a regressing edit is blocked (spec: skills)
- [ ] 72.5 Implement cost- and latency-aware routing policy; verify with routing table tests (spec: model-providers)
- [ ] 72.6 Implement surface toolsets from the session and taint-aware escalation; verify that tainted content raises the approval floor (spec: tool-system)
- [ ] 72.7 Implement terminal workspace checkpoints; verify that a checkpoint restores the workspace (spec: execution-environments)
- [ ] 72.8 Implement credential injection at the egress proxy and opt-in external secret sources; verify that injected credentials never reach the child process (spec: security-boundaries)
- [ ] 72.9 Implement natural-language schedule translation and the managed scheduler contract; verify against a mock managed scheduler (spec: scheduled-jobs)
- [ ] 72.10 Implement cross-platform identity linking, delivery/read receipts and busy acknowledgement via reactions; verify with the mock platform (spec: messaging-gateway, spec: platform-adapters)
- [ ] 72.11 Implement fan-in join modes for delegation; verify with mock-provider tests (spec: subagent-delegation)
- [ ] 72.12 Implement migration rollback and the plugin development loop; verify rollback restores the pre-migration home (spec: hermes-migration, spec: plugin-system)

## 73. M3 Additional upgrades: configuration, egress, runtimes and providers

- [ ] 73.1 Implement the remote configuration backend (unreachable plane fails closed), profile descriptions, profile distributions (updates keep user config, credentials never installed), identity migration and purge (purge snapshotted) and external secret source commands; verify with fixtures (spec: configuration-and-profiles)
- [ ] 73.2 Implement egress proxy operation and status, egress proxy coverage of containers and remote sandboxes (CA inside the container), the egress credential source with placeholder rotation, password manager login sources and the dependency vulnerability scan CI gate; verify with proxy integration tests (spec: security-boundaries)
- [ ] 73.3 Implement the Lightpanda browser engine, the bot screen (virtual display for the agent) and the bot screen takeover lease; verify that the agent waits during a user takeover (spec: execution-environments)
- [ ] 73.4 Implement the Codex app-server runtime (switch deferred to the next session) and the Codex runtime migration (user text preserved); verify with a stub Codex binary (spec: model-providers)
- [ ] 73.5 Implement API gateway proxy mode (container relays to the host agent) and the subscription proxy on `8645` (path allowlist, client bearer replaced, LAN clients need a key); verify with HTTP tests (spec: api-server)
- [ ] 73.6 Implement local-model context window and memory placement (window grows before compression) and local resource telemetry; verify with a stub server (spec: local-models)
- [ ] 73.7 Implement `video_generate` (stuck jobs time out) and provider-contributed media tools; verify with mock providers (spec: web-and-media-tools)
- [ ] 73.8 Implement shared install state retention for worktrees, the `/connectors` slash command and language switchers that show endonyms (RTL layout); verify with tests (spec: git-worktrees, spec: connectors, spec: i18n)
- [ ] 73.9 Implement the NeMo Relay and Langfuse integrations (no export without keys) and the embeddable client SDKs (protocol version negotiation); verify with fixtures (spec: observability, spec: workspace-architecture)

## 74. M3 Agent interop: A2A and peer gateways

- [ ] 74.1 Implement A2A server enablement and binding (off by default, wildcard binds need tokens, browser pages refused), the Agent Card and the A2A JSON-RPC methods with the task lifecycle; verify against the official A2A SDK (spec: agent-interop)
- [ ] 74.2 Implement A2A conversation identity, authentication, trust and rate limits, inbound safety (no slash commands from peers, ping-pong capped) and push notifications (private addresses refused); verify with peer fixtures (spec: agent-interop)
- [ ] 74.3 Implement the outbound `a2a_*` tools (unknown agent names refused), the peer gateway registry (keys never on the command line), peer direct messages with idempotent remote runs, the union roster across gateways and the desktop relay for cross-connection bot messages; verify with two daemon fixtures (spec: agent-interop)

## 75. M3 Bot Mode

- [ ] 75.1 Implement bots as profiles with backend metadata, the canonical Bot Chat (`/new` keeps the relationship, headless Bot Chats), creating, duplicating and deleting bots (OAuth never copied, remote creation), avatars and roster presentation with hiding and sections; verify with desktop and CLI tests (spec: bot-mode)
- [ ] 75.2 Implement bot voices, bot routines delivering into the Bot Chat and the bot protocol in canonical Bot Chats; verify with mock providers (spec: bot-mode)
- [ ] 75.3 Implement the `message_agent` tool (friendly names cannot hijack profiles, busy targets), typed delivery failures with safe retry, mentions from any chat and agent-to-agent messages from bot chats (forged calls outside a bot chat refused); verify with multi-profile tests (spec: bot-mode, spec: desktop-app)
- [ ] 75.4 Implement group chats (rooms do not spin, silent rounds settle), room stop directives with held members, the durable room driver, hosted rooms across gateways, room authority transfer and the Bot Mode off switch with CLI parity; verify with multi-profile and two-daemon tests (spec: bot-mode)

## 76. M3 Computer use

- [ ] 76.1 Implement the `computer_use` tool actions (background clicks, stale elements), capture modes with the payload budget and the screenshot context budget, approval of non-capture actions (refused in cron), hard-blocked keys and typed payloads, and permission modes fixed per runtime; verify with a mock driver (spec: computer-use)
- [ ] 76.2 Implement driver runtime management and identity (`cua-driver`, unsigned bundles refused), backend selection and availability, which machine is driven (container backends use `prometheus-sandbox-desktop`), platform prerequisites (e.g. Screen Recording permission) and the agent cursor overlay; verify per OS in CI where available (spec: computer-use)
- [ ] 76.3 Implement the computer-use CLI and doctor and screenshot retention and delivery to messaging platforms; verify with CLI tests and the fake adapter (spec: computer-use)

## 77. M3 Voice: streaming, barge-in, voice channels and wake word

- [ ] 77.1 Implement live streaming transcription (fallback on failure), barge-in (talk over playback, stop phrase mid-generation), the voice turn model and reasoning settings, and recording retention and privacy; verify with audio fixtures (spec: voice-mode)
- [ ] 77.2 Implement Discord voice channel sessions with echo suppression, access control and rebinding; verify against a mock Discord voice gateway (spec: voice-mode)
- [ ] 77.3 Implement the wake word listener (refused without TTS), wake engines with false-trigger control, single wake listener ownership across surfaces, wake capture location for remote clients, wake word profile routing and wake and voice diagnostics; verify with audio fixtures (spec: voice-mode)
- [ ] 77.4 Implement desktop voice controls, client-direct voice for remote daemons (audio never crosses the daemon link unless the provider is host-only) and the realtime full-duplex voice chat mode that delegates work; verify with desktop E2E tests (spec: voice-mode)

## 78. M3 Catalog platform adapters

- [ ] 78.1 Implement supervised sidecar adapters (crashes contained), the catalog adapter set (migration installs adapters disabled), adapter host forwarding mode and platform-contributed toolsets (absent on other platforms); verify with a sidecar fixture (spec: platform-adapters)
- [ ] 78.2 Implement the Telegram local Bot API server, Telegram network resilience and presentation options, Slack reaction triggers and multiple workspaces, the peer-agent routing self-test, Matrix end-to-end encryption (fails closed when required) and Matrix rooms, invites and clock skew; verify against mock servers (spec: platform-adapters)
- [ ] 78.3 Implement webhook event coalescing and MCP server events into the webhook pipeline (duplicate events ignored, placeholder injection inert); verify with event fixtures (spec: platform-adapters, spec: plugin-system)
- [ ] 78.4 Implement the WhatsApp Cloud API adapter, the Microsoft Graph change-notification listener, the Microsoft Teams adapter, the Teams meeting summary pipeline and the Google Chat adapter as signed catalog adapters; verify each with the conformance kit (spec: platform-adapters)
- [ ] 78.5 Implement the Mattermost, DingTalk, Feishu and Lark (including document comments and meeting invitations), WeCom smart robot, WeCom callback and Weixin adapters plus the Feishu and Yuanbao tools; verify each with the conformance kit (spec: platform-adapters)
- [ ] 78.6 Implement the BlueBubbles, Photon, QQ Bot, Yuanbao, LINE, SMS and ntfy adapters; verify each with the conformance kit (spec: platform-adapters)
- [ ] 78.7 Implement the IRC, Raft (wake-only), Buzz Nostr and SimpleX adapters, the Home Assistant adapter and toolset (dangerous domains blocked) and the Discord tools (allowlisted actions); verify each with the conformance kit (spec: platform-adapters)

## 79. M3 Plugin catalog and first-party plugins

- [ ] 79.1 Implement plugin collections (tag refs refused), Python package and Nix plugin sources (unpinned packages refused) and the catalog tool for agent-initiated installs (the model cannot pick a version); verify with catalog fixtures (spec: plugin-system)
- [ ] 79.2 Ship the first-party disk-cleanup and google_meet plugins and the catalog plugins for Home Assistant and Camofox; verify each plugin's scenarios in CI (spec: plugin-system)

## 80. M3 Pets and achievements

- [ ] 80.1 Implement pets as cosmetic, cache-neutral state, pet storage with the sprite format, pet gallery commands, pet configuration and scale, and the single activity-to-state mapping; verify that selecting a pet mid-session leaves the prompt unchanged (spec: pets)
- [ ] 80.2 Implement terminal pet rendering (off when output is redirected), pet preview and doctor, the pet slash command, generating a pet and affection reactions; verify with snapshot tests (spec: pets)
- [ ] 80.3 Implement the desktop pet with the pop-out overlay, achievements from session history (incremental, non-blocking scans) and achievement unlock celebrations; verify with desktop E2E and store tests (spec: pets)

## 81. M3 Exit criteria

- [ ] 81.1 M3 exit: the desktop app and the dashboard attach to one daemon and show the same live session, and an ACP editor fixture completes a prompt round trip on the same daemon (spec: desktop-app, spec: web-dashboard, spec: acp-adapter)
- [ ] 81.2 M3 exit: the offline self-improvement demo adopts a harness proposal, writes an experience entry, and a later session receives the lesson, with the answer-free tripwire test green (spec: harness-evolution, spec: experience-library, spec: replay-and-evaluation)
- [ ] 81.3 M3 exit: a kanban board dispatches two workers that complete dependent tasks, a bot group chat on the desktop settles after one round, an A2A peer completes a remote run, computer use drives a mock desktop under approval, a pet reacts in the TUI, and a catalog adapter passes the conformance kit; every M3 scenario across specs maps to a passing test in the scenario index; `openspec validate --all --strict` and `scripts/check` are green on main (spec: kanban, spec: bot-mode, spec: agent-interop, spec: computer-use, spec: pets, spec: platform-adapters)

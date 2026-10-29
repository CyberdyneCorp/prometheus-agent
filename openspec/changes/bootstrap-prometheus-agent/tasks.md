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

## 5. M1 Configuration, profiles and secrets

- [ ] 5.1 Implement home resolution (`--home` > `PROMETHEUS_HOME` > platform default) and the D13 layout created with modes 0700/0600, with every profile, `default` included, under `profiles/<name>/`; verify with temp-home tests that the layout, permissions and `active_profile` are correct (spec: configuration-and-profiles)
- [ ] 5.2 Implement `ProfileScope` (home path, secrets handle, config snapshot) as the required parameter of every profile-data API, with no global profile state; verify with a compile-fail test (trybuild) that store/config access without a scope does not compile (spec: configuration-and-profiles)
- [ ] 5.3 Define the typed `config.toml` schema (serde + schemars) as the single source of defaults for M1 sections, using snake_case keys, `_secs`/`_ms`/`_bytes` suffixes, typed enums and `schema_version`; verify that `cargo xtask gen` regenerates the config reference and that the docs list every M1 key with its default (spec: configuration-and-profiles)
- [ ] 5.4 Implement strict validation (unknown keys and invalid enum values are errors with path and allowed values, exit 78) and the layering (defaults < profile file < `--set`); verify with tests that the YAML-style `off` value is rejected for approval modes and that `--set` overrides are typed (spec: configuration-and-profiles)
- [ ] 5.5 Implement format-preserving atomic config writes (toml_edit, temp file + rename, comments kept) and the config CLI (`config get|set|unset|edit|path|validate`, plus `config explain <key>` showing the value source chain); verify with round-trip tests that comments survive and `explain` names the winning layer (spec: configuration-and-profiles)
- [ ] 5.6 Implement schema versioning with forward migrations and a `.bak.<timestamp>` backup; verify with a fixture `schema_version = 0` file that migrates and keeps a backup, and that a file newer than the binary is refused with exit 78 (spec: configuration-and-profiles)
- [ ] 5.7 Implement the secrets store (`secrets.toml` 0600 or OS keychain via `keyring`), the resolution precedence (env provider-native names / `PMT_SECRET_*` > store) scoped to the profile, and the `secrets set|list|rm` CLI that never echoes values; verify with tests that secrets never appear in config, logs or `--json` output (spec: configuration-and-profiles, spec: security-boundaries)
- [ ] 5.8 Implement profile name rules, the lifecycle commands (`profile list|create|use|rename|delete|show`), safe deletion (confirmation, refusal while the profile is in use) and profile selection at process start (`-p` > `active_profile` > `default`); verify with CLI tests that profiles share no state (islands) and that deleting the active profile is refused (spec: configuration-and-profiles)
- [ ] 5.9 Write the configuration and profiles user guide (home layout, secrets vs config, profiles) and verify that every command it quotes passes an automated docs-snippet test (spec: configuration-and-profiles)

## 6. M1 Session storage

- [ ] 6.1 Implement the `pmt-store` open path (only through `ProfileScope`, 0600 files, WAL, bounded readers, checkpoints, `store_meta.profile_id` binding, maximum readable schema) with embedded versioned migrations; verify that opening another profile's db fails and that a newer schema is refused with a clear error (spec: session-storage)
- [ ] 6.2 Implement the session and message tables (session record, source enum, first end reason wins, typed lineage: parent/continuation/fork/child) with transactional message persistence; verify with property tests that a crash mid-turn leaves either all of the turn's rows or none (spec: session-storage)
- [ ] 6.3 Implement the content-addressed blob store `blobs/<sha256>` with reference counting and garbage collection; verify that an unreferenced blob is collected and a referenced one is not (spec: session-storage)
- [ ] 6.4 Implement non-destructive in-place compaction rows, the turn lease and compaction lock; verify with concurrent tests that two compactions on one session serialize and that the original rows stay recoverable (spec: session-storage, spec: context-compression)
- [ ] 6.5 Implement session titles with provenance, lineage numbering (`Title #2`) and title/prefix/id resolution; verify with tests for ambiguous and exact matches (spec: session-storage)
- [ ] 6.6 Implement FTS5 full-text search with the trigram tokenizer, query sanitisation and derived indexes that fail open and rebuild offline; verify that hostile FTS syntax is neutralised and that a dropped index is rebuilt by `doctor --fix` (spec: session-storage)
- [ ] 6.7 Implement the startup integrity check with quarantine of a corrupt file, out-of-band replacement detection and the write-contention policy (busy timeout, then exit 75); verify with a corrupted fixture that it is quarantined and a fresh db is created (spec: session-storage)
- [ ] 6.8 Implement rewind as a soft delete, deletion semantics, retention/pruning/auto-archive, resume/export size guards and usage events keyed by root lineage; verify with tests that rewound messages are hidden but restorable and that retention prunes only eligible sessions (spec: session-storage)
- [ ] 6.9 Implement the `sessions` CLI (`list`, `show`, `search`, `rename`, `export`, `delete`, `prune`, `rewind`) with `--json`; verify with CLI snapshot tests, and document it in the session guide (spec: session-storage, spec: cli)

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

## 9. M1 Local OS sandbox and execution environment

- [ ] 9.1 Implement in `pmt-sandbox` the capability-to-policy translation and the Linux backend (Landlock fs plus TCP where the kernel supports it, seccomp filter), including denial of credential stores by default; verify with integration tests on Linux CI that a sandboxed `cat ~/.ssh/id_rsa` and a write outside the workspace both fail (spec: security-boundaries, spec: execution-environments)
- [ ] 9.2 Implement the macOS Seatbelt profile generator with the same policy; verify with macOS CI integration tests that mirror 9.1 (spec: security-boundaries)
- [ ] 9.3 Implement the Windows AppContainer + Job Object backend; verify with Windows CI tests (Tier 2) that a write outside the workspace fails, and report strength per feature (spec: security-boundaries)
- [ ] 9.4 Implement the enforcement-strength report (`fs`, `net`, `process`, `syscalls` → `enforced|partial|unavailable`) and `security.sandbox.allow_degraded`, with a WARN logged on every start while degraded; verify that an unavailable feature without `allow_degraded` refuses to run commands (spec: security-boundaries)
- [ ] 9.5 Implement the typed terminal backend registry (`local` built in; `container`/`ssh` names reserved and failing with a "ships in M2" error until group 30), readiness checks and terminal config defaults; verify that an unknown backend exits 78 with the available list (spec: execution-environments)
- [ ] 9.6 Implement the local backend: working-directory resolution, session cwd tracking, terminal argument validation, environment built from an allowlist, and pre-execution guards that fail closed; verify with tests that the parent's secrets are absent from the child env (spec: execution-environments)
- [ ] 9.7 Implement the foreground cap with promotion to background and the server guard, foreground result shaping, spilling full output to a redacted 0600 file, and infrastructure failures returned as degraded results; verify with tests on a long-running command and on a huge output (spec: execution-environments)
- [ ] 9.8 Implement the background process registry (`process` toolset: list, poll, kill, wait) with reaping on session end; verify that no orphan remains after the session closes (spec: execution-environments)

## 10. M1 Model providers: registry, wire formats and resolution

- [ ] 10.1 Implement the typed, profile-scoped provider registry and the descriptor contract (id, wire format, host rules, auth kinds, base URL, capabilities); verify that unknown and disabled providers fail with typed errors and exit 78 (spec: model-providers)
- [ ] 10.2 Implement the `chat_completions`, `responses` and `anthropic_messages` adapters normalised into one event stream (`text_delta` … one terminal `stop`), plus the wire-format selection order; verify with recorded HTTP fixtures (wiremock) per format and a host-rule test (spec: model-providers)
- [ ] 10.3 Implement requested-provider precedence without ambient env, auto detection, the rule that local endpoints never fall back to remote providers, and `prometheus model explain` for deterministic resolution; verify with resolution table tests (spec: model-providers)
- [ ] 10.4 Implement the model metadata catalog (context window, pricing, capabilities) and capability-aware route validation with a minimum context window; verify that a route lacking tool support for a tool-using session is rejected (spec: model-providers)
- [ ] 10.5 Implement error classification, same-route retries, fallback chain configuration, in-turn fallback activation with turn-scoped restore, and a stable cache prefix across route changes; verify with fault-injection tests (spec: model-providers, spec: agent-turn-loop)
- [ ] 10.6 Implement prompt caching on the request type (Anthropic `cache_control`, OpenAI prompt cache key) and per-call cost accounting into the ledger with `billing_source`; verify with fixture requests carrying the expected cache markers (spec: model-providers, spec: observability)
- [ ] 10.7 Implement auxiliary task routing (title, summary, vision …) where the auto route never guesses another account; verify that the auxiliary route uses only the main credential's provider unless configured (spec: model-providers)
- [ ] 10.8 Implement the `/model` switch (next turn, cache-break notice) and `prometheus model [list|set|info]`; verify with CLI tests (spec: model-providers, spec: cli)

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
- [ ] 13.7 Implement the `tools` introspection commands (`tools list|show|toolsets`) and the generated tool-name documentation; verify with CLI snapshots and docs freshness (spec: tool-system, spec: agent-turn-loop)

## 14. M1 Core tools: file, web, browser, todo and clarify

- [ ] 14.1 Implement `read_file`, `write_file`, `patch`, `search_files` and `list_dir` through the backend, with read guards, pagination, dedup, write guards and stale-overwrite protection; verify with tests that an edit after an out-of-band change is refused (spec: execution-environments)
- [ ] 14.2 Implement the `terminal` tool on top of group 9; verify that a tool call runs under the sandbox and returns shaped output (spec: execution-environments)
- [ ] 14.3 Implement the `web` toolset (search/extract via configured provider) with destination safety; verify with a mock HTTP server (spec: tool-system, spec: security-boundaries)
- [ ] 14.4 Implement browser automation with lazy Chromium/Node runtime install, session resolution, URL safety, supervision, dialog handling and cleanup; verify with a headless integration test against a local page (spec: execution-environments)
- [ ] 14.5 Implement the agent-level tools `todo`, `clarify` and `session_search` (owned by the turn loop; generic dispatch returns the "must be handled by the agent loop" error); verify with dispatch tests (spec: tool-system)

## 15. M1 Agent turn loop

- [ ] 15.1 Implement the turn entry contract, typed exit reasons, single active turn per session, and iteration phases in `pmt-agent`; verify with mock-provider tests for each exit reason (spec: agent-turn-loop)
- [ ] 15.2 Implement the layered system prompt (identity `SOUL.md`, context files, skills index, memory snapshot, platform hints) frozen per session, with request-time additions that never touch history; verify with a test asserting the system prompt bytes are identical across turns (spec: agent-turn-loop)
- [ ] 15.3 Implement project context discovery, context file scanning (threat scan) and the size cap; verify with fixtures that an injected instruction file is flagged and an oversized one is truncated (spec: agent-turn-loop)
- [ ] 15.4 Implement the single iteration limit, per-agent iteration budget and the budget-exhaustion summary handoff, wired to the budget engine of 7.7; verify the turn ends with the right reason after the handoff (spec: agent-turn-loop)
- [ ] 15.5 Implement interrupts and steering with placement that preserves the cache prefix; verify that an interrupt mid-stream ends the turn cleanly and that a steer message is appended after the last tool result (spec: agent-turn-loop)
- [ ] 15.6 Implement transcript repair before each request (orphan tool calls/results), model-call retries and auto-recovery, and the failed-turn boundary row; verify with fault-injection mock scripts (spec: agent-turn-loop)
- [ ] 15.7 Implement dependency-planned parallel tool batches from resource claims, with undeclared tools run sequentially; verify that two reads run in parallel and two writes to one path do not (spec: agent-turn-loop)
- [ ] 15.8 Implement cache-hit reporting and `cache.unexpected_break` detection using the canonical prefix hash, plus step events per iteration into the trajectory; verify that an injected prefix change emits the event (spec: agent-turn-loop, spec: observability)

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
- [ ] 17.6 Ship the essential `prometheus-agent` skill and the `skills` CLI (`list|show|enable|disable|eject|lint`); verify CLI snapshots and document skill authoring (spec: skills, spec: cli)

## 18. M1 Memory

- [ ] 18.1 Implement the built-in memory store as rows in `state.db` (typed entries, core and archival tiers, core character budgets) and the frozen system-prompt snapshot refreshed only at compression or a new session; verify that a mid-session write does not change the prompt (spec: memory)
- [ ] 18.2 Implement the `memory` tool (targets, operations, `new_text` alias, `replace` refusal), validation before any write, atomic batches judged on the final state, entry matching, and the per-turn retry cap; verify with tool tests for each action (spec: memory)
- [ ] 18.3 Implement threat scanning of memory content, store enable flags, write approval with pending changes, the rule that background review cannot delete unattended, the nudge interval, and per-turn recall injection as a fenced ephemeral block; verify that recall never persists into history (spec: memory)
- [ ] 18.4 Implement the `memory` CLI (`list|show|edit|export|import`) with Markdown as the edit/export view; verify a round trip export → edit → import (spec: memory)

## 19. M1 CLI line interface

- [ ] 19.1 Implement the clap command tree with global flags (`-p`, `--home`, `--set`, `--json`, `--no-color`, `-v`, `-q`, `-V`), `chat` as the default, and stable exit codes; verify CLI snapshot tests and generated CLI reference (spec: cli)
- [ ] 19.2 Implement `chat` flags, one-shot (`-z`/`--oneshot`) and piped mode, and TTY-first interface selection (`ui.interface`; M1 ships `line`); verify that `echo hi | prometheus chat --provider mock` prints `mock: hi` and exits 0 (spec: cli)
- [ ] 19.3 Implement the single typed slash-command registry, command resolution (prefix, alias, typo hint), precedence for non-built-in commands, and the M1 built-in set; verify that `/help` is generated from the registry (spec: cli)
- [ ] 19.4 Implement busy-policy enforcement (`busy.input_mode`), deferred effects with `--now`, destructive-command confirmation, skill invocation as a user message and quick commands; verify with line-interface tests driven by expectrl (spec: cli)
- [ ] 19.5 Implement interactive input (multiline, history, Ctrl-C semantics), themes as data, and usage display; verify with expectrl tests (spec: cli)
- [ ] 19.6 Implement the setup wizard (provider choice incl. subscription sign-in, writes config and secrets to the right stores); verify with a scripted run on a temp home (spec: cli, spec: configuration-and-profiles)
- [ ] 19.7 Implement `prometheus doctor` with the check groups, `--online`, `--json`, the enumerated `--fix` repairs and `doctor security`; verify that each repair is printed and that no network request happens without `--online` (spec: workspace-architecture, spec: cli, spec: security-boundaries)

## 20. M1 Distribution and self-update

- [ ] 20.1 Configure `cargo-dist` for every target in D11 with checksums, minisign signatures, SPDX SBOM, provenance attestation and a signed `release-manifest.json`; verify with a dry-run release in CI that every artifact is produced and verified (spec: distribution-and-update)
- [ ] 20.2 Implement the release identity grammar (incl. canary `vX.Y.Z-canary.<ts>`), the embedded trust root (primary and standby keys) and signature verification with no skip flag; verify that a tampered archive fails with `signature verification failed` (spec: distribution-and-update, spec: security-boundaries)
- [ ] 20.3 Ship shell and PowerShell installers that write the install receipt (owner, channel); verify the installers in CI containers (spec: distribution-and-update)
- [ ] 20.4 Implement `prometheus update [--check] [--channel]` with telemetry-free checks, exit 10 for an available update, the single-writer update lock, refusal for installs owned elsewhere (exit 69 naming the owner), atomic swap with a health check, update receipts and `--rollback`; verify with a local release server fixture (spec: distribution-and-update)
- [ ] 20.5 Implement schema migrations around updates (pre-migration snapshot so rollback is safe); verify that an update followed by a rollback reopens the old store (spec: distribution-and-update, spec: session-storage)
- [ ] 20.6 Implement the managed runtime store (hash-pinned index, atomic rename), the lazy installation policy, `prometheus runtimes` CLI, leases with GC and the download retry budget; verify that a digest mismatch deletes the download and fails (spec: distribution-and-update)
- [ ] 20.7 Add distribution checks to doctor and reproducible locked builds (`--locked`, a rebuild diff); verify that two CI builds of the same commit produce identical digests on Linux musl (spec: distribution-and-update)
- [ ] 20.8 Write the install and update guide and verify the documented commands in CI (spec: distribution-and-update)

## 21. M1 Offline demo and exit criteria

- [ ] 21.1 Implement `prometheus demo [--output DIR] [--keep]` with the bundled ledger scenario and mock script, run in a temporary profile; verify that it prints canonical trajectory steps, `demo passed`, reports `Total: 85.15` and exits 0 within 30 s offline (spec: replay-and-evaluation)
- [ ] 21.2 M1 exit: in CI, on every Tier 1 target and with networking disabled, `prometheus demo` passes end to end and the Tier 2 targets pass the offline smoke run (spec: replay-and-evaluation, spec: workspace-architecture)
- [ ] 21.3 M1 exit: every M1 scenario in workspace-architecture, configuration-and-profiles, session-storage, observability, security-boundaries, execution-environments, model-providers, tool-system, agent-turn-loop, context-compression, skills, memory, cli and distribution-and-update maps to a passing test (a scenario-to-test index checked by `scripts/check`)
- [ ] 21.4 M1 exit: the startup, memory and size gates pass on the reference runner, and `scripts/check` is green on main

## 22. M2 Daemon and client RPC protocol

- [ ] 22.1 Implement `pmt-gateway` daemon start/stop/status with one daemon per home, the discovery file and local control channel, and `daemon ensure --json`; verify that a second daemon start finds the first and exits non-zero (spec: workspace-architecture, spec: messaging-gateway)
- [ ] 22.2 Implement ticket-authenticated loopback WebSocket attachment and the shared close-code table (1000…4426); verify that a bad ticket closes with 4401 and a bad Origin with 4403 (spec: workspace-architecture)
- [ ] 22.3 Implement JSON-RPC 2.0 framing, the error-code table, `rpc.hello` handshake and versioning (major mismatch → -32008/4426); verify with protocol conformance tests (spec: workspace-architecture)
- [ ] 22.4 Generate `schema/rpc.json` and TypeScript bindings from the Rust types and add them to the freshness check; verify that changing a param type without regenerating fails CI (spec: workspace-architecture)
- [ ] 22.5 Implement strict param validation, profile and ownership checks on every session method, server-to-client requests, event sequencing with reconnect replay, liveness/backpressure, and stale-build detection; verify with tests for replay after a dropped connection (spec: workspace-architecture)
- [ ] 22.6 Implement `prometheus rpc` (debug client); verify that it can call `rpc.hello` and a session method (spec: workspace-architecture)
- [ ] 22.7 Implement CLI daemon attachment with in-process fallback and a single interactive owner per session; verify that two CLIs attaching to one session get an ownership error for the second (spec: cli)
- [ ] 22.8 Implement live config reload with apply classes, the managed configuration layer, profile alias launchers, the served profile set and profile export without credentials by default; verify with tests that a reload marked `next_session` does not alter a live session (spec: configuration-and-profiles)
- [ ] 22.9 Add shell completions (`prometheus completions <shell>`); verify generated completions load in bash and zsh (spec: cli)
- [ ] 22.10 Document the client protocol (generated reference plus guide) and verify the docs build (spec: workspace-architecture)

## 23. M2 TUI

- [ ] 23.1 Implement the ratatui TUI launch selection and attachment to the daemon as a client, plus standalone in-process mode; verify with a pty test that `prometheus` on a TTY with `ui.interface = "tui"` attaches (spec: tui)
- [ ] 23.2 Implement terminal state restoration on every exit path (panic, signal) and the in-TUI update hand-off; verify with pty tests that the terminal mode is restored after SIGTERM (spec: tui)
- [ ] 23.3 Implement the TUI side of the protocol (handshake, typed method registry, error and close codes, attachment-scoped session access, sequencing and replay, liveness, non-blocking long requests, retirement, server requests with fail-closed deadlines, idempotent creation, busy submission, author stamping, profile scope); verify against the shared conformance suite (spec: tui)
- [ ] 23.4 Implement the slash command flow, discovery RPCs that preserve the prompt cache, composer with path completion, status line with accessibility, configurable key bindings and orphan attachment reaping; verify with snapshot tests (spec: tui)

## 24. M2 Messaging gateway core

- [ ] 24.1 Implement profiles served by one daemon, routing identity with deterministic session keys, DM/group/thread isolation, and single-flight routing; verify with table tests of session keys (spec: messaging-gateway)
- [ ] 24.2 Implement the busy input policy from config only, command busy policy from the registry, FIFO queued turns, stop preserving the session, and conversations that never rotate on time; verify with gateway tests (spec: messaging-gateway)
- [ ] 24.3 Implement gateway slash commands, quick commands, typed lifecycle hooks and command access tiers; verify that a user without a tier gets a refusal (spec: messaging-gateway)
- [ ] 24.4 Implement the authorization order with a fail-closed default and the pairing flow (`prometheus pairing`); verify that an unpaired user is refused and a paired one is admitted (spec: messaging-gateway)
- [ ] 24.5 Implement approval requests routed to every surface, streaming to platforms, background/internal notifications and a durable delivery outbox; verify with a fake adapter (spec: messaging-gateway)
- [ ] 24.6 Implement crash recovery with a clean-shutdown marker, stuck-loop suspension, agent cache and eviction and the all-adapters-down policy; verify by killing the daemon mid-turn and checking recovery (spec: messaging-gateway)
- [ ] 24.7 Implement the relay connector, content-free opt-in health export, per-user rate limits and per-chat policies; verify with a relay fixture and rate-limit tests (spec: messaging-gateway)
- [ ] 24.8 Document the gateway (setup, pairing, policies) and verify commands with the docs-snippet test (spec: messaging-gateway)

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

## 26. M2 WASM plugin host

- [ ] 26.1 Define the WIT worlds (`tool`, `hook`, `provider`, `platform-adapter`, `memory-provider`, `context-engine`, `terminal-backend`, `ui-panel`, `delegation-router`) with versioning rules; verify that a sample component per world builds with `cargo component` (spec: plugin-system)
- [ ] 26.2 Implement `plugin.toml` manifest parsing (kinds `wasm`/`mcp`/`python`), discovery sources and identity, and opt-in enablement; verify that a discovered plugin stays disabled until enabled (spec: plugin-system)
- [ ] 26.3 Implement capability declarations and user-approved, digest-bound grants pinned to plugin identity; verify that a changed digest invalidates the grant (spec: plugin-system, spec: security-boundaries)
- [ ] 26.4 Implement the wasmtime host with fuel, epoch interruption and memory caps, host interfaces, profile-scoped plugin state and isolation; verify that an infinite loop is interrupted within the bound (spec: plugin-system)
- [ ] 26.5 Implement plugin tool registration, the typed hook registry (closed list), versioned payloads, hook isolation and time bounds, `pre_tool_call` directives and `pre_llm_call` context that preserves the prompt cache; verify that an unknown hook name fails at load (spec: plugin-system)
- [ ] 26.6 Implement bounded frozen system-prompt sections, plugin slash commands, the event bus, plugin-bundled skills, the LLM access gate and fail-closed provider plugins; verify with sample plugins (spec: plugin-system)
- [ ] 26.7 Implement load order, dependencies and deadlines, hot reload that respects the prompt-cache invariant, and safe mode (`--safe-mode`); verify that reload defers tools to the next session unless `--now` (spec: plugin-system)
- [ ] 26.8 Implement the plugin catalog, kill list, signed packages with provenance and `--allow-unsigned` recorded, and the `plugins` CLI; verify that an unsigned catalog install is refused by default (spec: plugin-system, spec: security-boundaries)
- [ ] 26.9 Write the plugin authoring guide with a working example plugin and verify the example builds and loads in CI (spec: plugin-system)

## 27. M2 MCP client and server

- [ ] 27.1 Implement the MCP client (`rmcp`, stdio under the sandbox and streamable HTTP) configured per profile with `prometheus mcp add|list|rm|test`; verify against a reference MCP server fixture (spec: plugin-system)
- [ ] 27.2 Implement MCP tool naming and filtering plus runtime safety (timeouts, size bounds, crash handling); verify with a misbehaving server fixture (spec: tool-system)
- [ ] 27.3 Implement MCP server mode exposing Prometheus tools and skills under grants; verify with an MCP client conformance test (spec: plugin-system)
- [ ] 27.4 Implement tool-search progressive disclosure for large MCP surfaces; verify that only the search tool is exposed until a tool is selected (spec: tool-system)

## 28. M2 Python bridge

- [ ] 28.1 Implement the managed CPython runtime (uv-provisioned) and per-plugin pinned environments; verify a pinned environment is created offline from a cache fixture (spec: python-bridge)
- [ ] 28.2 Implement the sidecar JSON-RPC wire protocol, typed values across the wire, one profile per sidecar and the lifecycle; verify with an echo plugin (spec: python-bridge)
- [ ] 28.3 Implement the supported `PluginContext` subset, named errors for unsupported APIs, Hermes import shims, hook name/payload translation and the static compatibility audit; verify with real Hermes example plugins as fixtures (spec: python-bridge)
- [ ] 28.4 Enforce grants at the process boundary (sandbox + egress proxy), secrets/env isolation, call timeouts with stall detection, crash isolation with restart policy and resource limits; verify that a crashing plugin does not affect the agent (spec: python-bridge, spec: security-boundaries)
- [ ] 28.5 Implement Python skill scripts, registration reload rules, `prometheus bridge` diagnostics and migration of Hermes plugins; verify with an end-to-end Hermes plugin test (spec: python-bridge)

## 29. M2 Security hardening for extensions

- [ ] 29.1 Implement the loopback egress proxy that resolves DNS itself and the per-session egress policy for child processes; verify that a sandboxed child can reach only allowed hosts (spec: security-boundaries)
- [ ] 29.2 Ensure gateway-owned secrets can never be delegated to tools, children, skills or plugins, and implement plugin/sidecar isolation checks; verify with grant tests (spec: security-boundaries)
- [ ] 29.3 Implement the tamper-evident security audit log (`security audit verify`) and the security posture check; verify that editing a record breaks verification (spec: security-boundaries)
- [ ] 29.4 Implement tool result provenance tagging, approval explain/dry-run and smart approvals with a denial breaker; verify with approval tests (spec: tool-system)

## 30. M2 Execution environments: containers, SSH and remote

- [ ] 30.1 Implement the `container` backend (rootless Podman/Docker, `--cap-drop ALL`, `no-new-privileges`, read-only root, tmpfs), hardening that is never silently dropped, and lifecycle with orphan reaping; verify with container integration tests (spec: execution-environments)
- [ ] 30.2 Implement the `ssh` backend, reserved built-in names and the backend isolation descriptor; verify against an SSH server container (spec: execution-environments)
- [ ] 30.3 Implement remote sandboxes as `terminal-backend` plugins; verify with a sample plugin backend (spec: execution-environments)
- [ ] 30.4 Implement throttled background notifications, subagent process handoff, file snapshots with undo, and local browser isolation; verify that `/undo` restores a file (spec: execution-environments)

## 31. M2 Subagent delegation

- [ ] 31.1 Implement `delegate_task` (contract, spawn preconditions, task validation, config defaults, role from depth) with children in isolated sessions and a capability grant that can only narrow; verify that a child cannot gain a tool the parent lacks (spec: subagent-delegation)
- [ ] 31.2 Implement named subagent definitions, child model routing and fail-closed child approvals; verify with mock-provider tests (spec: subagent-delegation)
- [ ] 31.3 Implement background and synchronous dispatch, the concurrency limit with fair queueing, durable background completions and stall detection; verify that a background child's result survives a daemon restart (spec: subagent-delegation)
- [ ] 31.4 Implement control actions scoped to the caller's spawn tree, stop semantics, live progress streaming, the structured result contract, `output_schema` validation, the summary budget, fan-out budget split/roll-up and the one-shot spawn budget; verify with tree-scoped tests (spec: subagent-delegation)
- [ ] 31.5 Implement fail-closed delegation routers, delegation settings changes that respect caching, profile routes into the target `ProfileScope`, the plugin lifecycle interface and child reaping; verify with tests (spec: subagent-delegation)
- [ ] 31.6 Implement programmatic tool calling (`execute_code`) under the caller's grant with its RPC channel, limits and pre-execution guards; verify that an empty grant intersection is refused (spec: subagent-delegation)

## 32. M2 Scheduled jobs

- [ ] 32.1 Implement the schedule grammar (cron, interval, one-shot) with canonical echo and confirmation, payload validation, job records and state literals; verify with grammar table tests (spec: scheduled-jobs)
- [ ] 32.2 Implement next-run computation, grace and missed occurrences, and at-most-once firing; verify with a fake clock across a daemon restart (spec: scheduled-jobs)
- [ ] 32.3 Implement the built-in trigger in the daemon and the headless tick, execution concurrency and timeouts, fresh isolated sessions, the unattended memory policy and fail-closed approvals; verify with integration tests (spec: scheduled-jobs)
- [ ] 32.4 Implement script-backed and no-agent jobs, the prompt safety scan, pre-dispatch preflight, delivery targets via the durable outbox, and response framing and silence; verify with a fake adapter (spec: scheduled-jobs)
- [ ] 32.5 Implement run history and replay, lifecycle ops, repeat limits and retention, the pluggable trigger provider, the `cron` CLI and model tools, and the cron doctor; verify with CLI tests and document cron (spec: scheduled-jobs)

## 33. M2 Turn loop, compression, memory and storage upgrades

- [ ] 33.1 Implement unpriced-model handling under a cost budget, the budget checkpoint notice and structured steering; verify with the spec scenarios (spec: agent-turn-loop)
- [ ] 33.2 Implement the deterministic turn journal and crash resume mid-tool-batch (re-run only `effect = none` or idempotent calls, otherwise "outcome unknown"); verify by killing the process mid-batch and resuming (spec: agent-turn-loop)
- [ ] 33.3 Implement install-tree context suppression and subdirectory hints that ride tool results; verify with fixtures (spec: agent-turn-loop)
- [ ] 33.4 Implement host validation of engine output, per-model threshold overrides, gateway session hygiene, retrievable pruned content, the summary quality check, pinned messages, micro-compaction, prepared-ahead summaries and proactive prune; verify with golden transcripts (spec: context-compression)
- [ ] 33.5 Implement the memory revision journal, reviewable changes per session and semantic recall; verify that a change can be reverted (spec: memory)
- [ ] 33.6 Implement the external memory provider contract (one provider, isolated and bounded, gated tools and prompt block, egress redaction and mirroring, profile-bound, pre-compress checkpoint, loud degradation, setup writing to the right stores) and context-engine plugins; verify with a sample WASM provider (spec: memory)
- [ ] 33.7 Implement the session forking API and portable session export/import; verify a round trip between two homes (spec: session-storage)

## 34. M2 Additional subscription providers and routing

- [ ] 34.1 Implement provider registry changes at session boundaries, minted short-lived credentials, and OpenRouter OAuth key minting; verify with mock OAuth servers (spec: model-providers)
- [ ] 34.2 Implement xAI SuperGrok, Nous Portal, Qwen Portal and MiniMax portal sign-ins; verify each with mock IdP fixtures (spec: model-providers)
- [ ] 34.3 Implement plugin OAuth (host-run PKCE, opaque credential handle) and `external_process` providers; verify with a sample provider plugin (spec: model-providers, spec: plugin-system)
- [ ] 34.4 Implement plan and quota visibility, billing-source transparency, scoped fallback chains for pinned owners, provider health scoring (`providers health`) and provider spend caps; verify with tests (spec: model-providers)

## 35. M2 Replay and model-call cache

- [ ] 35.1 Implement the canonical request hash (excluding transport, streaming and cache-control fields) and the content-addressed model-call cache with admin commands and limits; verify that equivalent requests hash identically (spec: replay-and-evaluation)
- [ ] 35.2 Implement session recordings, replay modes, divergence detection and side-effect-free replay; verify that replaying a recorded session offline reproduces the trajectory (spec: replay-and-evaluation)
- [ ] 35.3 Add the golden-recording regression gate and the built-in smoke suite to `scripts/check`; verify that a behavior change in the loop fails the gate (spec: replay-and-evaluation)

## 36. M2 Observability upgrades

- [ ] 36.1 Implement opt-in OpenTelemetry export, the span model, trace context propagation and the metrics catalog with an optional local scrape endpoint; verify against an OTLP collector container (spec: observability)
- [ ] 36.2 Implement live usage events, health endpoints (`/healthz`, `/readyz`), the component health model and `prometheus status [--check]`; verify with daemon tests (spec: observability)
- [ ] 36.3 Implement the crash/debug bundle (redacted) and the observability overhead budget gate; verify that the bundle contains no secrets (spec: observability)

## 37. M2 Skills hub and curator

- [ ] 37.1 Implement sandboxed skill scripts with managed runtimes and the hub install pipeline with trust policy; verify with a local hub fixture (spec: skills)
- [ ] 37.2 Implement the skills lockfile with tamper detection, signatures and provenance, and blueprint skills that never auto-schedule; verify that a modified installed skill is detected (spec: skills)
- [ ] 37.3 Implement the curator (configuration, scheduling, transitions, scope, backups, CLI); verify with curator tests (spec: skills)

## 38. M2 Distribution channels

- [ ] 38.1 Publish package-manager and image channels (Homebrew tap, WinGet, container image, Nix flake) with receipts naming the owner; verify that self-update is refused for each with exit 69 (spec: distribution-and-update)
- [ ] 38.2 Implement running-daemon handover on update, update dry-run with changelog preview, offline bundles and `prometheus uninstall`; verify with a local release server (spec: distribution-and-update)

## 39. M2 Hermes migration

- [ ] 39.1 Implement `prometheus migrate from-hermes` surface, the read-only source guarantee, profile discovery/mapping, dry run and mapping report; verify against a fixture Hermes home that the source tree hash is unchanged (spec: hermes-migration)
- [ ] 39.2 Implement config translation from user-set values only (supported versions, fail-closed ambiguous YAML, env references), `.env` re-sorting into secrets and config, credential files and typed-name validation; verify with fixture configs (spec: hermes-migration)
- [ ] 39.3 Implement streaming verified session import with lineage and usage events, memory and identity import, and skills import with scanning; verify counts and FTS search after import (spec: hermes-migration)
- [ ] 39.4 Implement cron jobs imported paused, MCP servers and plugins (bridge), gateway settings and pairing allowlists; verify with fixtures (spec: hermes-migration)
- [ ] 39.5 Implement idempotent incremental re-runs, per-component atomicity and resumability, exit codes and confirmation; verify that a second run imports nothing new and an interrupted run resumes (spec: hermes-migration)
- [ ] 39.6 Write the migration guide and verify its commands against the fixture home (spec: hermes-migration)

## 40. M2 Exit criteria

- [ ] 40.1 M2 exit: an end-to-end test starts the daemon, attaches the TUI and a CLI client, sends a message through the mock platform adapter, receives a streamed reply, and resumes after a dropped connection with no lost events (spec: workspace-architecture, spec: tui, spec: messaging-gateway)
- [ ] 40.2 M2 exit: a WASM plugin, an MCP server and a Hermes Python plugin each register a tool that the agent calls under grants, and an ungranted capability is refused in each (spec: plugin-system, spec: python-bridge)
- [ ] 40.3 M2 exit: a cron job delegating to a background subagent fires exactly once across a daemon restart and delivers via the outbox (spec: scheduled-jobs, spec: subagent-delegation)
- [ ] 40.4 M2 exit: migrating the fixture Hermes home and running the golden replay suite pass in CI; every M2 scenario across specs maps to a passing test in the scenario index

## 41. M3 Web dashboard

- [ ] 41.1 Implement `prometheus dashboard` on `127.0.0.1:9120` with the SPA embedded in the binary, listener lifecycle within the daemon and a configurable reverse-proxy base path; verify that the binary serves the SPA offline (spec: web-dashboard)
- [ ] 41.2 Implement loopback auth with an HttpOnly session cookie, CSRF protection, fail-closed non-loopback binds, Host validation, same-origin only and the public endpoint list; verify with HTTP security tests (spec: web-dashboard)
- [ ] 41.3 Implement profile-addressed routes, structured chat over the client protocol, observer-by-default remote users and the live event stream; verify with Playwright E2E tests (spec: web-dashboard)
- [ ] 41.4 Implement the built-in pages, write-only secrets, schema-driven config editing, the honest platform matrix, redacted bounded logs, upload/download limits, and the cost, harness-evolution and experience views; verify with Playwright tests (spec: web-dashboard)

## 42. M3 Desktop app

- [ ] 42.1 Scaffold the Tauri 2 app in `apps/desktop` reusing the dashboard components, with daemon attachment through the bundled binary (`daemon ensure` bounded), local endpoint validation and version-skew restart; verify with a desktop E2E test (spec: desktop-app)
- [ ] 42.2 Implement one-use dial tickets kept out of webview storage, shared client defaults and reconnect with full-jitter backoff; verify that tickets never appear in webview storage (spec: desktop-app)
- [ ] 42.3 Implement the native command allowlist, windows that never open themselves, the external URL scheme allowlist and deep links; verify with Tauri tests (spec: desktop-app)
- [ ] 42.4 Implement desktop-sourced sessions, the gone-session verdict, the slash palette from the registry, atomic bot chats, validated settings patches and explicit profile switching; verify with desktop E2E tests (spec: desktop-app)
- [ ] 42.5 Implement background running after windows close, approval notifications that never approve, isolated plugin UI panels and the global hotkey quick-ask; verify that a notification click opens the card without approving (spec: desktop-app)
- [ ] 42.6 Implement signed in-app updates on `stable`/`canary` and no updater in dev builds, plus honest platform support in settings; verify that a tampered update is rejected (spec: desktop-app, spec: distribution-and-update)

## 43. M3 ACP adapter

- [ ] 43.1 Implement `prometheus acp` launch flags, stdout-only protocol frames, quiet liveness probes and config from flags/profile only, as a daemon viewer bound to the launch profile; verify with an ACP client test that stdout carries only frames (spec: acp-adapter)
- [ ] 43.2 Implement initialize, auth reuse, new/load/resume/list/fork sessions and editor MCP servers owned by the daemon session; verify with ACP conformance fixtures (spec: acp-adapter)
- [ ] 43.3 Implement prompt submission with exactly-once submit, scoped cancel, unknown outcome after a daemon restart, slash commands, edit-approval modes, validated config options, model switching, fail-closed approval/clarify bridging, exactly-once reply projection and terminal tool statuses; verify with fault-injection tests (spec: acp-adapter)
- [ ] 43.4 Implement content conversion with media staging, metadata projection, editor-native file access and terminal delegation; verify with an editor fixture (spec: acp-adapter)

## 44. M3 Kanban

- [ ] 44.1 Implement board storage at `<home>/kanban/`, slugs and selection, the typed workflow table, task creation (idempotent) and dependency links; verify with store tests (spec: kanban)
- [ ] 44.2 Implement ready promotion with atomic claim, typed blocks with the loop breaker, scheduled parking, completion gates, the review lane and archive terminating the worker; verify with concurrency tests (spec: kanban)
- [ ] 44.3 Implement the single dispatcher per home, its tick, concurrency caps, the fail-closed claim allowlist, the failure circuit breaker, worker spawn by run ticket and liveness heartbeats; verify that a dead worker's task is reclaimed (spec: kanban)
- [ ] 44.4 Implement the kanban toolset and grants, workspaces, attachments, notifications through the owning profile, per-task cost budget, eval/evolution tasks, event log and run history, the CLI and board export/import; verify with CLI tests and document kanban (spec: kanban)

## 45. M3 Harness evolution

- [ ] 45.1 Implement opt-in gating, the harness tree layout, editable surface with component numbering, size caps, validation that collects every error (exit 65) and loading once per session; verify `prometheus harness validate` on fixtures (spec: harness-evolution)
- [ ] 45.2 Implement execution confinement, history/diff (`harness log|show|diff`), rollback and manual edits; verify that a rollback writes a new version (spec: harness-evolution)
- [ ] 45.3 Implement task capture, on-demand/scheduled/automatic triggers, modes with budget k → R=4k rollouts and P=3k improver sessions, and labels; verify the budget arithmetic with tests (spec: harness-evolution)
- [ ] 45.4 Implement parallel and sequential modes with isolated rollouts, side-effect recorders for `effect = external` tools, improver sessions, stage acceptance/retry/fallback and the evidence context; verify with the mock provider (spec: harness-evolution)
- [ ] 45.5 Implement answer-free prompts, leak globs with the verdict tripwire, the budget ledger and spend cap, and concurrency limits; verify that a planted rubric file aborts the job (spec: harness-evolution)
- [ ] 45.6 Implement blind anonymised selection, user-driven selection, proposals with adoption policy (approval required for scripts, hooks and subagents; never widening capabilities), stale-proposal protection, job lifecycle/events/RPC, resume with the content-addressed session cache and human-only evaluation; verify with end-to-end mock jobs (spec: harness-evolution)

## 46. M3 Experience library

- [ ] 46.1 Implement the library layout, domain names and resolution, the reflection workspace and distillation; verify with a mock-provider job (spec: experience-library)
- [ ] 46.2 Implement write-on-adoption only, entry contents, answer-free and secret-free entries, the index, exposure to proposers and stream order; verify that a rejected proposal writes no entry (spec: experience-library)
- [ ] 46.3 Implement lessons offered to later sessions, rollback demotion, the `experience` CLI, the retention cap, export/import between profiles, restore on resume and repair, and library events; verify with CLI tests (spec: experience-library)

## 47. M3 Evaluation suites

- [ ] 47.1 Implement the eval suite format and `prometheus eval run` with resumable runs; verify a suite run against the mock provider (spec: replay-and-evaluation)
- [ ] 47.2 Implement the answer-free boundary, post-hoc judging, re-judging with judge identity, and reports with comparison; verify that no grader data reaches the agent's context (spec: replay-and-evaluation)
- [ ] 47.3 Implement the benchmark adapter protocol and session-to-task capture; verify with a sample adapter (spec: replay-and-evaluation)
- [ ] 47.4 Implement the offline self-improvement demo; verify that it runs a full evolution job with the mock provider offline and exits 0 (spec: replay-and-evaluation, spec: harness-evolution)

## 48. M3 Remaining upgrades

- [ ] 48.1 Implement memory decay/expiry/dormancy and consolidation proposals; verify with fake-clock tests (spec: memory)
- [ ] 48.2 Implement hybrid semantic + full-text session search and the encryption-at-rest option; verify search ranking fixtures and that an encrypted db is unreadable without the key (spec: session-storage)
- [ ] 48.3 Implement TUI split panes, inline diff review, the timeline scrubber and shared live sessions; verify with pty snapshot tests (spec: tui)
- [ ] 48.4 Implement skill evals and eval-gated skill edits; verify that a regressing edit is blocked (spec: skills)
- [ ] 48.5 Implement cost- and latency-aware routing policy; verify with routing table tests (spec: model-providers)
- [ ] 48.6 Implement surface toolsets from the session and taint-aware escalation; verify that tainted content raises the approval floor (spec: tool-system)
- [ ] 48.7 Implement terminal workspace checkpoints; verify that a checkpoint restores the workspace (spec: execution-environments)
- [ ] 48.8 Implement credential injection at the egress proxy and opt-in external secret sources; verify that injected credentials never reach the child process (spec: security-boundaries)
- [ ] 48.9 Implement natural-language schedule translation and the managed scheduler contract; verify against a mock managed scheduler (spec: scheduled-jobs)
- [ ] 48.10 Implement cross-platform identity linking, delivery/read receipts and busy acknowledgement via reactions; verify with the mock platform (spec: messaging-gateway, spec: platform-adapters)
- [ ] 48.11 Implement fan-in join modes for delegation; verify with mock-provider tests (spec: subagent-delegation)
- [ ] 48.12 Implement migration rollback and the plugin development loop; verify rollback restores the pre-migration home (spec: hermes-migration, spec: plugin-system)

## 49. M3 Exit criteria

- [ ] 49.1 M3 exit: the desktop app and the dashboard attach to one daemon and show the same live session, and an ACP editor fixture completes a prompt round trip on the same daemon (spec: desktop-app, spec: web-dashboard, spec: acp-adapter)
- [ ] 49.2 M3 exit: the offline self-improvement demo adopts a harness proposal, writes an experience entry, and a later session receives the lesson, with the answer-free tripwire test green (spec: harness-evolution, spec: experience-library, spec: replay-and-evaluation)
- [ ] 49.3 M3 exit: a kanban board dispatches two workers that complete dependent tasks, and every M3 scenario across specs maps to a passing test in the scenario index; `openspec validate --all --strict` and `scripts/check` are green on main

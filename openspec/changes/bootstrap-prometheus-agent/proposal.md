# Proposal: Bootstrap Prometheus Agent

## Why

CyberdyneCorp needs a personal/team AI agent harness that matches what Hermes Agent (Nous Research) does today — one agent core across CLI, TUI, messaging gateway, desktop and web, with memory, skills, delegation, scheduling and real terminal/browser control — but ships as a single, fast, memory-safe Rust binary with a sandboxed extension model. Hermes' Python core pays for its flexibility with slow cold starts, in-process plugins that can do anything, a large dependency/installer surface and env-var/config drift. Prometheus re-specifies the same capability set from Hermes' baseline specs, fixes the drift those specs surfaced, and adds a self-improvement layer adapted from Harness² (Propose–Probe–Compose harness evolution, a per-domain experience library, blind answer-free output selection, and deterministic replay).

## What Changes

- New Rust project `prometheus-agent` (binary `prometheus`, short alias `pmt`) with a Cargo workspace; no code exists yet — this change defines the full target behavior.
- Full functional parity with the 23 Hermes capability areas (agent loop, compression, providers, tools, delegation, sandboxes, session storage, CLI, config/profiles, gateway, platform adapters, TUI, desktop, web dashboard, ACP, skills, memory, plugins, cron, kanban, distribution/update, security), re-specified for Rust and with the known Hermes inconsistencies resolved by design (single config source of truth, no non-secret env vars, fail-closed approvals, typed contracts between processes).
- Upgrades beyond Hermes:
  - **Sandboxed plugins**: WebAssembly component plugins with explicit capability grants, plus first-class MCP (client and server), plus a Python bridge that runs existing Hermes plugins and skills out-of-process.
  - **Harness evolution**: per-task Propose–Probe–Compose refinement of the agent's own harness (prompt, guardrails, tool guidance, skills) with a fixed rollout/session budget, without ground-truth answers.
  - **Experience library**: transferable lessons distilled after tasks and offered to later sessions in the same domain.
  - **Replay and evaluation**: content-addressed model-call cache, record/replay of trajectories, deterministic offline mock provider, and benchmark/eval suites runnable from the CLI.
  - **Observability**: one trajectory grammar for every surface, OpenTelemetry traces/metrics, and a per-session cost and token ledger with hard budgets.
  - **Capability-based security**: every tool, plugin and subagent runs under an explicit capability set; OS sandboxing (Landlock/seccomp on Linux, Seatbelt on macOS, AppContainer on Windows) for local execution; signed releases and signed plugins.
  - **Hermes migration**: one-shot import of a Hermes home (config, `.env`, sessions, memory, skills, cron jobs, profiles).
- **Milestones**: every requirement is tagged `M1` (core single-user agent), `M2` (multi-surface + extensibility) or `M3` (desktop, dashboard, kanban, self-improvement, evals).

## Capabilities

### New Capabilities

- `workspace-architecture`: crate layout, single binary, supported targets, async runtime model, error taxonomy, cross-process typed contracts, code-health gates.
- `agent-turn-loop`: turn lifecycle, iteration/cost budgets, interrupts/steering, retries, layered system prompt, identity file, context files, prompt-cache invariant.
- `context-compression`: thresholds, summarisation algorithm, in-place compaction, micro-compaction, cache breakpoints, manual compaction.
- `model-providers`: provider registry and plugins, runtime resolution, fallback chains, credential pools, auxiliary model routing, model metadata, model switching.
- `tool-system`: tool registry, availability gating, toolsets, dispatch, result budgets, approvals, MCP client tools, tool search.
- `subagent-delegation`: delegated child agents, isolation, background runs, lifecycle API, programmatic tool calling.
- `execution-environments`: terminal backends (local, container, SSH, remote sandboxes, plugins), process registry, file tools, browser automation.
- `session-storage`: durable session/message store, lineage, titles, full-text search, rewind, repair, retention, usage, export/import.
- `cli`: command-line entry points, slash-command registry and dispatch, deferred vs immediate changes, themes, setup wizard, logs.
- `configuration-and-profiles`: home resolution, typed config with schema/migration, secrets store, profiles as isolated islands.
- `messaging-gateway`: gateway process, session keys, busy handling, authorization/pairing, streaming, notifications, multi-profile serving, relay.
- `platform-adapters`: adapter contract, supported platforms, message-length limits, media, speech in/out, dedup/batching.
- `tui`: full-screen terminal UI and its attachment protocol to the local gateway.
- `desktop-app`: desktop application, gateway attachment, slash palette, bot mode, plugin UI SDK, updates.
- `web-dashboard`: browser dashboard, authentication, embedded terminal chat, live event streams.
- `acp-adapter`: Agent Client Protocol server for editors.
- `skills`: skill format (agentskills.io compatible), discovery, gating, installation, authoring, curation.
- `memory`: built-in memory store, memory tool, nudges, memory-provider and context-engine extensions.
- `plugin-system`: WebAssembly component plugins, capability grants, hooks, provider plugins, catalog and signing, MCP server mode.
- `python-bridge`: out-of-process Python runtime that hosts Hermes-compatible plugins and skills.
- `scheduled-jobs`: cron/interval/one-shot jobs, at-most-once firing, delivery, managed-scheduler contract.
- `kanban`: shared task board, dependencies, dispatcher, worker liveness and failure breaker.
- `distribution-and-update`: installers, release channels, signed self-update, managed runtimes (Node/Python/browsers) and lockfile policy.
- `security-boundaries`: trust boundaries, capability model, OS sandboxing, secret handling, egress control, redaction, telemetry opt-in.
- `harness-evolution`: per-task Propose–Probe–Compose harness refinement and blind answer-free output selection.
- `experience-library`: per-domain library of adopted harnesses and distilled lessons.
- `replay-and-evaluation`: model-call cache, trajectory record/replay, mock provider, eval suites and benchmark adapters.
- `observability`: trajectory grammar, structured logs, OpenTelemetry traces/metrics, cost and token ledger.
- `hermes-migration`: import of an existing Hermes home into Prometheus.

### Modified Capabilities

<!-- None: the repository has no existing specs. -->

## Impact

- New repository content: Cargo workspace, `openspec/` specs, CI (build, test, clippy, fmt, `openspec validate --all --strict`).
- External dependencies chosen in `design.md` (async runtime, SQLite, WebAssembly runtime, TUI/desktop frameworks).
- No impact on Hermes Agent; `hermes-migration` reads a Hermes home read-only.

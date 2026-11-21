# Prometheus Agent

A personal and team AI agent harness in Rust, by CyberdyneCorp.

One agent core runs behind every surface: CLI, full-screen TUI, messaging gateway, web dashboard, editor integration (ACP), an OpenAI-compatible API and a desktop app. It remembers across sessions, writes and reuses skills, delegates to subagents, runs scheduled jobs, drives a real terminal and browser inside OS sandboxes, and can refine its own harness through a budgeted Propose–Probe–Compose loop.

It ships as one signed binary, `prometheus` (alias `pmt`). Extensions run as sandboxed WebAssembly components with explicit capability grants, as MCP servers, or as existing Hermes Python plugins through an out-of-process bridge.

> **Status: M0–M2 done, M3 in progress.** The core agent (M1) and the multi-surface and extension layer (M2) are merged. From M3, the web dashboard, the desktop app, ACP, A2A, kanban, harness evolution, the experience library, eval suites and the upgrades (memory, search, TUI, routing, egress, runtimes, providers, the Lightpanda browser engine and the bot screen) are in; bot mode, computer use, voice upgrades and pets are in review, and the catalog platform adapters, the plugin catalog and the M3 exit criteria are next (75 of 101 tasks ticked). The full target behavior is specified in [`openspec/changes/bootstrap-prometheus-agent/`](openspec/changes/bootstrap-prometheus-agent/).

## Why Prometheus

| | |
|---|---|
| **Fast and small** | Native binary, startup budget 20 ms, no Python or Node runtime required |
| **Safe extensions** | WASM plugins only get the capabilities you grant; local commands run under Landlock/seccomp, Seatbelt or AppContainer |
| **Your subscriptions** | Claude Max, ChatGPT/Codex, SuperGrok, Nous Portal and more by sign-in, or API keys, or local models |
| **Self-improving** | Memory, skills, an experience library and answer-free harness evolution |
| **Reproducible** | Every model call can be recorded and replayed; offline mock provider and eval suites |
| **Hermes-compatible** | Imports a Hermes home and runs Hermes plugins and skills; [every Hermes feature](docs/hermes-parity.md) is mapped |

## Screenshots

Captured from the real `prometheus` binary with the offline `mock` provider and an isolated home: no API key, no network. The scenario is the one [`prometheus demo`](docs/guide/demo.md) runs, where the agent fixes the balance bug in a small ledger repository.

<p align="center">
  <img src="docs/images/tui-demo.png" width="820" alt="The full-screen TUI: the agent reads the README, runs total.sh, reads the script, patches the debit sign and re-runs it, ending with Total: 85.15; a status bar shows the profile, model, context use and cost">
  <br><sub>The full-screen TUI (<code>prometheus tui</code>) after the agent fixed the ledger bug.</sub>
</p>

<p align="center">
  <img src="docs/images/dashboard-chat.png" width="820" alt="The web dashboard's chat page with the same task: agent messages interleaved with read_file, terminal and patch tool calls, each marked done">
  <br><sub>The web dashboard (<code>prometheus dashboard</code>): the same task driven from the browser, with its tool calls.</sub>
</p>

<p align="center">
  <img src="docs/images/dashboard-sessions.png" width="410" alt="The dashboard's sessions page listing six sessions from the dashboard, the TUI and the CLI, with search and delete">
  <img src="docs/images/dashboard-skills.png" width="410" alt="The dashboard's skills page: the profile's bundled skills with name, category and description">
  <br><sub>Sessions from every surface in one list, and the profile's skill catalog.</sub>
</p>

<p align="center">
  <img src="docs/images/cli-status.png" width="720" alt="A terminal running prometheus status (daemon running, dashboard listener, scheduler, mock provider health) and prometheus sessions list">
  <br><sub><code>prometheus status</code> and <code>prometheus sessions list</code> against the running daemon.</sub>
</p>

## Features

```mermaid
mindmap
  root((Prometheus))
    Agent core
      Turn loop with budgets
      Context compression
      Subagents and worktrees
      goal, loop and heartbeat
      Mixture of agents
    Models
      Provider registry and fallback
      Subscription sign-in
      Credential pools
      Local models
    Tools
      Terminal and files
      Browser and computer use
      Web and media
      LSP diagnostics
      Connectors
    Surfaces
      CLI and TUI
      Messaging gateway
      Web dashboard
      Desktop app
      ACP for editors
      OpenAI-compatible API
      Voice mode
    Learning
      Memory
      Skills and curator
      Experience library
      Harness evolution
    Extensions
      WASM components
      MCP client and server
      Python bridge
      Plugin catalog
    Automation
      Scheduled jobs
      Kanban board
      Bot mode
      A2A and peers
    Trust
      Capability grants
      OS sandboxes
      Secret redaction
      Signed releases
      Replay and evals
```

## Architecture

One daemon per Prometheus home owns the agent, storage and plugins. Every client attaches to it over a loopback WebSocket JSON-RPC protocol, so a session started in Telegram can be continued in the TUI or the desktop app.

```mermaid
flowchart TB
    subgraph Clients
        CLI[CLI / TUI]
        DESK[Desktop app]
        WEB[Web dashboard]
        ACP[Editors via ACP]
        API[OpenAI-compatible API]
        MSG[Telegram, Discord, Slack,<br/>Matrix, Email, ...]
    end

    subgraph Daemon["prometheus daemon (one per home)"]
        RPC[JSON-RPC over loopback WebSocket]
        GW[Messaging gateway]
        AGENT[Agent turn loop<br/>compression, delegation]
        TOOLS[Tool registry<br/>grants and approvals]
        SCHED[Cron and kanban]
        LEARN[Memory, skills,<br/>experience library]
        OBS[Trajectories, OTel,<br/>cost ledger]
    end

    subgraph Extensions
        WASM[WASM components]
        MCP[MCP servers]
        PY[Python bridge]
    end

    subgraph Outside
        PROV[Model providers<br/>subscriptions, keys, local]
        SBX[OS sandbox<br/>local, container, SSH, remote]
        DB[(SQLite + FTS5<br/>per profile)]
    end

    CLI & DESK & WEB & ACP & API --> RPC
    MSG --> GW
    RPC & GW & SCHED --> AGENT
    AGENT --> TOOLS & LEARN & OBS
    AGENT --> PROV
    TOOLS --> SBX
    TOOLS --> WASM & MCP & PY
    AGENT & LEARN & OBS --> DB
```

The workspace mirrors this split. Each crate owns one area:

| Crate | Owns |
|---|---|
| `prometheus` | The binary: CLI, daemon entry point |
| `pmt-core` | Shared types, ids, error taxonomy, exit codes |
| `pmt-config` | Typed TOML config, profiles, secrets store |
| `pmt-store` | SQLite sessions, search, retention |
| `pmt-providers` | Model providers, credentials, routing |
| `pmt-agent` | Turn loop, prompt, compression |
| `pmt-tools`, `pmt-sandbox` | Tool registry, execution environments, OS sandboxes |
| `pmt-skills`, `pmt-memory` | Skills and memory |
| `pmt-obs` | Trajectories, logs, traces, cost ledger |
| `pmt-plugins`, `pmt-mcp`, `pmt-pybridge` | WASM host, MCP, Python bridge |
| `pmt-gateway`, `pmt-tui`, `pmt-web`, `pmt-acp` | Surfaces |
| `pmt-cron`, `pmt-kanban` | Scheduling and the task board |
| `pmt-evolve`, `pmt-eval` | Harness evolution, replay and evals |
| `pmt-migrate` | Hermes and other agent importers |

Design decisions are in [`design.md`](openspec/changes/bootstrap-prometheus-agent/design.md).

## Use cases

```mermaid
flowchart LR
    DEV([Developer]) --> T1[Pair-program in the terminal<br/>or editor, in a worktree]
    DEV --> T2[Delegate a goal and<br/>let it loop until done]
    TEAM([Team]) --> T3[Shared bot in Slack, Discord<br/>or Telegram with pairing]
    TEAM --> T4[Kanban board worked<br/>by agent workers]
    OPS([Operator]) --> T5[Scheduled reports, audits<br/>and backups delivered to chat]
    APP([Application]) --> T6[OpenAI-compatible API<br/>or A2A from other agents]
    RES([Researcher]) --> T7[Record, replay and evaluate<br/>runs; evolve the harness]
    HER([Hermes user]) --> T8[Import a Hermes home<br/>and keep the plugins]
```

## Roadmap

| Milestone | Delivers | Tasks | Status |
|---|---|---|---|
| **M0** Scaffolding | Workspace, `scripts/check`, CI on Tier 1/2 targets, health and perf gates, generated references | 12 | Done |
| **M1** Core agent | Config and profiles, sessions, providers and subscription sign-in, tools, sandbox, turn loop, compression, skills, memory, CLI, self-update; offline `prometheus demo` | 154 | Done |
| **M2** Multi-surface and extensibility | Daemon and RPC, TUI, messaging gateway and adapters, WASM/MCP/Python plugins, delegation, worktrees, cron, voice, local models, API server, Hermes migration | 204 | Done |
| **M3** Desktop, dashboard, self-improvement | Desktop app, dashboard, ACP, kanban, harness evolution, experience library, evals, A2A, bot mode, computer use | 101 | In progress |

Each milestone ends with exit tests run in CI. Task-level detail is in [`tasks.md`](openspec/changes/bootstrap-prometheus-agent/tasks.md).

## Quick start (development)

Needs Rust (`rustup` installs the pinned toolchain), Node 22 for the OpenSpec CLI, and [`just`](https://github.com/casey/just).

```sh
git clone https://github.com/CyberdyneCorp/prometheus-agent.git
cd prometheus-agent
just tools          # install the pinned cargo-deny, nextest, mdbook, openspec
just build
just run --version  # prometheus 0.1.0 (<channel>, <target>, <commit>, <build date>)
just check          # every gate CI runs
```

| Command | Does |
|---|---|
| `just build` / `just release` | Debug workspace build / optimized binary |
| `just run ARGS` | Run `prometheus` |
| `just test` | Tests (nextest) and doctests |
| `just fmt` / `just lint` | Format / clippy with warnings denied |
| `just check` | Every CI gate (`scripts/check`) |
| `just check-fast` | Every gate except perf budgets |
| `just gen` | Regenerate `schema/` and `docs/reference/` |
| `just health` / `just perf` | Code-health budgets / startup, memory and size budgets |
| `just docs` / `just docs-serve` | Build / serve the docs site |
| `just install` | Install `prometheus` and `pmt` into `~/.cargo/bin` |
| `just hook` | Run `scripts/check` on every `git push` |

## Contributing

Prometheus is specified before it is built: behavior lives in OpenSpec, and a change that alters behavior updates its spec in the same pull request.

1. Pick a task from [`tasks.md`](openspec/changes/bootstrap-prometheus-agent/tasks.md) and read the spec it names.
2. Branch from `main`, implement with tests, and tick the task when its verification passes.
3. Run `just check` (or install the hook with `just hook`).
4. Open a pull request with a descriptive body; CI runs `scripts/check` on every Tier 1 target.

Rules that CI enforces: no `unsafe` outside allowlisted modules, cognitive complexity ≤ 15 per function, ≤ 120 lines per function, ≤ 1,500 per file, the dependency license allowlist, and fresh generated files. Details in [CONTRIBUTING.md](CONTRIBUTING.md); report vulnerabilities as described in [SECURITY.md](SECURITY.md).

## Documentation

| Where | What |
|---|---|
| [`proposal.md`](openspec/changes/bootstrap-prometheus-agent/proposal.md) | Scope and the 44 capabilities |
| [`design.md`](openspec/changes/bootstrap-prometheus-agent/design.md) | Architecture decisions and shared contracts |
| [`specs/`](openspec/changes/bootstrap-prometheus-agent/specs/) | One behavior spec per capability, requirements tagged M1–M3 |
| [`docs/`](docs/) | Docs site; `docs/reference/` is generated from code |
| [`docs/hermes-parity.md`](docs/hermes-parity.md) | Every Hermes feature mapped to its Prometheus requirement |
| [`perf/README.md`](perf/README.md) | Performance budgets and baselines |

## License

TBD.

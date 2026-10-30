# Prometheus Agent

A personal and team AI agent harness written in Rust by CyberdyneCorp.

Prometheus runs one agent core across a CLI, a full-screen TUI, a messaging gateway (Telegram, Discord, Slack, Email, Matrix, Signal, WhatsApp, webhooks), a web dashboard, editor integration over ACP and a desktop app. It learns across sessions through memory and skills, delegates to subagents, runs scheduled jobs, drives a real terminal and browser inside OS sandboxes, and can refine its own harness through a budgeted, answer-free Propose–Probe–Compose loop.

It ships as a single signed binary (`prometheus`, alias `pmt`). Extensions run as sandboxed WebAssembly components with explicit capability grants, as MCP servers, or as Hermes-compatible Python plugins through an out-of-process bridge.

## Status

Specification phase. The full target behavior is defined as an OpenSpec change:

- `openspec/changes/bootstrap-prometheus-agent/proposal.md`: scope and capability list
- `openspec/changes/bootstrap-prometheus-agent/design.md`: architecture decisions
- `openspec/changes/bootstrap-prometheus-agent/specs/`: one behavior spec per capability, each requirement tagged with a milestone (M1 core agent, M2 multi-surface and extensibility, M3 desktop, dashboard and self-improvement)
- `openspec/changes/bootstrap-prometheus-agent/tasks.md`: implementation plan by milestone
- `docs/hermes-parity.md`: every Hermes Agent feature mapped to the Prometheus requirement that specifies it

Validate locally:

```sh
npm install -g @fission-ai/openspec@1.13.2
openspec validate --all --strict
```

## Build

The Rust workspace lives under `crates/` (toolchain pinned in `rust-toolchain.toml`):

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
scripts/dev-install            # installs prometheus and its pmt alias into ~/.cargo/bin
cargo xtask perf               # startup, memory and size budgets (perf/README.md)
```

## License

TBD.

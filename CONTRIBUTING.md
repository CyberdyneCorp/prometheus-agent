# Contributing to Prometheus Agent

Prometheus is specified before it is built. Behavior lives in the OpenSpec
change under `openspec/changes/bootstrap-prometheus-agent/`, and every change
to behavior updates the matching spec in the same pull request.

## Setup

The Rust toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on
the first `cargo` call. The gates also need these exact tool versions (the
pins live at the top of `scripts/check`, which fails up front on a mismatch
and prints the install command):

```sh
cargo install --locked cargo-deny@0.20.2
cargo install --locked cargo-nextest@0.9.148
cargo install --locked mdbook@0.5.4
npm install -g @fission-ai/openspec@1.13.2
```

`just tools` installs the same pinned versions, and `just` (with no
arguments) lists shortcuts for building, testing and running the gates; the
recipes in `justfile` call the same commands as `scripts/check`.

## One command: `scripts/check`

```sh
scripts/check
```

It runs every blocking gate, in this order, and stops at the first failure,
naming it:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
3. `cargo nextest run --workspace --all-features --locked --no-tests=fail`
4. `cargo test --workspace --all-features --locked --doc`
5. `cargo deny check` (license allowlist and known advisories, `deny.toml`)
6. `openspec validate --all --strict --no-interactive`
7. `cargo xtask gen --check` (generated contracts and reference docs are fresh)
8. `mdbook build docs` (any warning fails)
9. `cargo xtask health` (the code-health rules below)
10. `cargo xtask perf` (startup, memory and size budgets, below)

CI (`.github/workflows/ci.yml`) runs `scripts/check --no-perf` on every Tier 1
target, so a green local run predicts a green check job. The budgets are
defined for one reference runner, so CI runs them only there, with
`scripts/check --perf` against main's baseline; timing on the shared matrix
runners is too noisy to gate on. CI also builds the Tier 2 targets (Windows
x86_64 and aarch64, Android aarch64).

### Pre-push hook

```sh
scripts/check --install-hook pre-push
```

This makes every `git push` run `scripts/check` first. It refuses to replace a
hook it did not write; rerun it to refresh its own hook.

### Generated files

The JSON Schemas in `schema/` and the pages in `docs/reference/` are generated
from the Rust types. Never edit them by hand; regenerate and commit the
result:

```sh
cargo xtask gen
```

`gen` and `gen --check` also check tool names in the docs against the tool
registry: a hand-written page names a tool as a link to its reference entry
(`` [`todo`](../reference/tools.md#todo) ``), and an unregistered name in a
tool context (`` the `todo_list` tool ``) or one that extends a registered
name (`todo_list` for `todo`) fails, in hand-written and generated pages
alike. A page that documents another product's tools opts out with
`<!-- tool-refs: off -- <why> -->`.

### Performance budgets

Startup, peak memory and binary size have absolute budgets and a 15%
regression limit (`perf/README.md`). A local `scripts/check` runs the gate
last (`--no-perf` skips it); run it alone before touching startup paths or dependencies:

```sh
scripts/check --perf        # same as cargo xtask perf
```

## Code-health rules

`cargo xtask health` enforces, per function:

- cognitive complexity ≤ 15 (≤ 30 in parser and codec modules listed under
  `parser_modules` in `health.toml`)
- ≤ 120 lines
- nesting ≤ 5

and per file ≤ 1,500 lines.

`unsafe` is forbidden outside the modules listed under `unsafe_modules` in
`health.toml` (OS sandbox and FFI only), and every `unsafe` block or
`unsafe impl` needs a `// SAFETY:` comment directly above it explaining why it
is sound.

Code that already exceeds a limit is listed in `health.toml` as an
`[[exception]]` with a reason. Exceptions may only shrink: a new entry, a
raised `max` or code that grows past its `max` fails the gate, and a `max`
looser than the code must be lowered. Split the function instead of adding an
exception.

## Pull requests

- Keep each pull request to one concern, and update the spec or docs when
  behavior changes.
- Every bug fix comes with a regression test that fails without the fix.
- Commit messages say what changed and why.
- Security issues are not reported in public pull requests or issues; see
  [SECURITY.md](SECURITY.md).

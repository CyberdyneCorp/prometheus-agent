# Shortcuts for everyday work. `scripts/check` stays the source of truth for
# what CI enforces; these recipes call the same commands so nothing drifts.

set shell := ["bash", "-euo", "pipefail", "-c"]

# List the recipes.
default:
    @just --list --unsorted

# --- build and run ---------------------------------------------------------

# Debug build of the whole workspace.
build:
    cargo build --workspace --locked

# Optimized build of the prometheus binary.
release:
    cargo build --release --locked -p prometheus

# Run the prometheus binary with ARGS (e.g. `just run --version`).
run *ARGS:
    cargo run --locked -p prometheus -- {{ARGS}}

# Install prometheus and its pmt alias into ~/.cargo/bin (or BIN_DIR).
install *BIN_DIR:
    scripts/dev-install {{BIN_DIR}}

# --- test and lint ---------------------------------------------------------

# Unit and integration tests (nextest), then doctests.
test *ARGS:
    cargo nextest run --workspace --all-features --locked {{ARGS}}
    cargo test --workspace --all-features --locked --doc

# Format the code.
fmt:
    cargo fmt --all

# Clippy with warnings denied, as CI runs it.
lint:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# Licenses and security advisories of every dependency.
deny:
    cargo deny check

# Complexity, size and unsafe-code budgets (health.toml).
health:
    cargo xtask health

# Validate the OpenSpec specs and changes.
spec:
    openspec validate --all --strict --no-interactive

# --- gates -----------------------------------------------------------------

# Every gate CI runs, perf budgets included. Run this before pushing.
check:
    scripts/check

# Every gate except the perf budgets (what CI's platform jobs run).
check-fast:
    scripts/check --no-perf

# Startup, memory and size budgets against perf/baseline.json.
perf *ARGS:
    scripts/check --perf {{ARGS}}

# Record this platform's perf baseline (the absolute budgets still apply).
perf-baseline:
    scripts/check --perf --update-baseline

# Run scripts/check before every git push.
hook:
    scripts/check --install-hook pre-push

# --- generated files and docs ----------------------------------------------

# Regenerate schema/ and docs/reference/ from the code.
gen:
    cargo xtask gen

# Build the docs site into target/docs.
docs: gen
    mdbook build docs

# Serve the docs site with live reload.
docs-serve: gen
    mdbook serve docs --open

# --- setup -----------------------------------------------------------------

# Install the exact tool versions pinned in scripts/check.
tools:
    #!/usr/bin/env bash
    set -euo pipefail
    pin() { sed -n "s/^readonly $1=//p" scripts/check; }
    cargo install --locked "cargo-deny@$(pin CARGO_DENY_VERSION)"
    cargo install --locked "cargo-nextest@$(pin CARGO_NEXTEST_VERSION)"
    cargo install --locked "mdbook@$(pin MDBOOK_VERSION)"
    npm install -g "@fission-ai/openspec@$(pin OPENSPEC_VERSION)"

# Remove build output.
clean:
    cargo clean

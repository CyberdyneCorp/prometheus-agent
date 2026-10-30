# Security Policy

> Skeleton. The declared trust model (which mechanisms are security boundaries
> and which are heuristics) and the full list of in-scope findings are written
> in milestone M1, group 8 of
> `openspec/changes/bootstrap-prometheus-agent/tasks.md`, per the
> `security-boundaries` spec.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub's private vulnerability
reporting on this repository (Security → Report a vulnerability). Never put
them in a public issue, pull request, commit or comment. Reports are handled
under a 90-day coordinated-disclosure window.

## Trust model

To be written in M1 group 8.

## How the codebase guards itself

These checks run in `scripts/check` on every push (with the pre-push hook) and
on every pull request in CI; see [CONTRIBUTING.md](CONTRIBUTING.md).

- **No unsafe code by default.** The workspace lint policy forbids
  `unsafe_code` (`Cargo.toml`). `cargo xtask health` allows `unsafe` only in
  the modules listed under `unsafe_modules` in `health.toml` (OS sandbox and
  FFI), and every `unsafe` block or `unsafe impl` must carry a `// SAFETY:`
  comment.
- **Dependencies.** `cargo deny check` enforces the license allowlist and fails
  on known security advisories (`deny.toml`).
- **Pinned toolchain and CI.** The Rust toolchain is pinned in
  `rust-toolchain.toml`, gate tools are pinned in `scripts/check`, and every
  GitHub Action is pinned to a full commit SHA with least-privilege
  permissions.

Run the same checks locally:

```sh
scripts/check
cargo xtask health
cargo deny check
```

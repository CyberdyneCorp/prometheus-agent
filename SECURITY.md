# Security Policy

This document declares what Prometheus treats as a security boundary, what
it treats as a heuristic layered on top, which findings are vulnerabilities
and how to report them. It is the scope authority for triage (spec:
`security-boundaries`, Declared trust model; In-scope findings and private
reporting).

## Reporting a vulnerability

Report in-scope findings (below) **privately** through GitHub's private
vulnerability reporting on this repository (Security → Report a
vulnerability). Never put them in a public issue, pull request, commit or
comment. Include the version (`prometheus version --json`), the platform,
the boundary crossed and a reproduction.

Reports are handled under a **90-day coordinated-disclosure window**: we
confirm receipt, agree on a fix and a disclosure date, and publish an
advisory when the fix ships or when the 90 days end, whichever comes first.

Findings that are out of scope (hardening of a heuristic, see below) are
ordinary public work: open a normal issue or pull request.

## Trust model

Prometheus runs model-driven actions and third-party code. Unlike its
predecessor, that code runs **inside an enforced boundary by default**.
Everything else that filters, flags or rewrites what the agent does is a
heuristic: useful, but never what stands between an attacker and the user.

### Security boundaries

Only these mechanisms are security boundaries:

1. **The OS sandbox around local child processes**: every command, script,
   tool helper or server the agent starts locally runs confined to the
   session's capability set (Landlock and seccomp on Linux, Seatbelt on
   macOS, AppContainer and a Job Object on Windows), with credential stores
   (`~/.ssh`, `~/.aws`, `~/.gnupg`, keychains, the Prometheus home) denied
   by default.
2. **Container and remote backends**: terminal backends that run commands
   in a container or on a remote host, isolated from the host user account.
3. **WASM plugin instances**: plugins run in a WebAssembly instance that
   can reach only the capabilities the user granted to that plugin.
4. **Out-of-process sidecars**: the Python bridge and MCP stdio servers run
   in their own sandboxed processes, never inside the agent's process.
5. **Authenticated external surfaces**: the dashboard, API, webhook and
   messaging adapters and the local RPC socket admit only authenticated or
   allowlisted senders; no listener binds a non-loopback address without
   explicit configuration.

`prometheus doctor security` prints, for the active profile, which of these
boundaries are enforced on the current host and at what level (`full`,
`partial:<features>`, `none`).

Supporting these boundaries are the **capability model** (a closed
vocabulary of `fs.read`, `fs.write`, `net.connect`, `net.listen`,
`process.spawn`, `secrets.read`, `llm.call`, `tools.call`,
`tools.override`, `ui.*` and `browser.real_profile`; resolution can only
narrow, never widen), **grants** (`<profile>/grants.toml`, reviewable with
`prometheus grants list` and revocable with `prometheus grants revoke`),
and **fail-closed prompts** (a capability request that nobody answers, that
is dismissed, whose interface detaches or that times out after
`approvals.timeout_secs` is denied; text returned by tools can never answer
a prompt).

### Heuristics

These mechanisms reduce risk but are **not** security boundaries; a way
around one of them, while the boundaries above still hold, is a hardening
issue:

- **The approval gate**: dangerous-command patterns, hardline blocks and
  approval modes.
- **Output redaction**: replacing known secret values (and their
  URL-encoded and base64 forms) and credential-shaped strings with
  `[REDACTED:<NAME>]` / `[REDACTED]` in tool results, logs, trajectories,
  session storage and errors.
- **Skill scanning**: static checks of skills before they are installed.
- **Provenance markers**: labels that mark tool output and fetched content
  as untrusted data in the model's context.
- **Taint tracking**: following untrusted content through the session.

For example, a shell command that evades the dangerous-command patterns but
is still confined by the sandbox is a public hardening issue, not a private
advisory.

## In scope (report privately)

- **Sandbox escape**: escape from an enforced OS sandbox, container or WASM
  instance.
- **Ungranted capability**: use of a capability that was not granted
  (including a resolution that widened a subagent's, plugin's or tool's
  set).
- **Surface authorization bypass**: reaching the agent, an approval or its
  output through an external surface without passing its authorization.
- **Credential exfiltration through a protection**: a secret leaving
  through a mechanism meant to prevent it (secret scoping, the egress
  policy, surface-only secrets).
- **Signature bypass**: signature-verification bypass for plugins or
  releases.
- **Silent downgrade**: a hardening downgrade that happened without the
  explicit configuration the security spec requires (for example a
  container started without `no-new-privileges` and without an
  `allow_degraded` entry, or a sandbox feature missing without a WARN and a
  `security.downgrade` event).

## Out of scope (public hardening)

- Bypasses of a heuristic (approval patterns, redaction, skill scanning,
  provenance markers, taint tracking) while the boundaries hold.
- Behaviour the user explicitly configured (for example
  `security.sandbox.mode = "off"`, `security.redact_secrets = false` or a
  broad grant), which Prometheus reports at WARN on every start.
- Issues that require an attacker who already controls the user account or
  the Prometheus home.

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

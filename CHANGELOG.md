# Changelog

Release notes, newest first. The release pipeline signs them into each
release's `release-manifest.json`, and `prometheus update --dry-run`
prints the entries between the installed and the target version. Each
release is a `## vX.Y.Z` heading with any of the subsections
`### Breaking`, `### Features`, `### Fixes` and `### Security`, each a
list of `- ` entries; `## Unreleased` entries ship with the next canary.

## Unreleased

### Features

- Package-manager and image channels: Homebrew tap, WinGet, Termux, Nix
  flake (with NixOS and Home Manager modules) and the OCI image.
- `prometheus update --dry-run` previews the changelog, migrations and
  runtime pin changes; offline bundles (`prometheus bundle create`,
  `update --from-bundle`); `prometheus uninstall`.
- `prometheus migrate from-hermes` re-runs incrementally through a
  per-profile ledger (edits made in Prometheus are kept as conflicts) and
  resumes after Ctrl+C; importers for OpenClaw, Claude Code, Codex CLI and
  Cursor with `migrate detect`, `migrate sync`, `migrate cleanup openclaw`
  and `migrate sessions` for Claude Code and Codex CLI conversations.

## v0.1.0

### Features

- First release: the single-user agent (CLI, providers, tools, skills,
  memory, sessions), signed self-update and managed runtimes.

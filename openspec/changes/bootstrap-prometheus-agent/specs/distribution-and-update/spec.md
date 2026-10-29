# Distribution and Update

## Purpose
Distribution covers how Prometheus reaches a machine and stays current: one signed binary per target shipped through installers, package managers, a container image and offline bundles; `stable` and `canary` release channels; a signed, atomic self-update with health check and rollback; and lazily installed, hash-pinned external runtimes (Node, CPython, Chromium) in a shared tool store. The key decision is that the binary replaces Hermes' source checkout, virtualenv and package manager: the only things that change on update are one file and, when needed, pinned runtime archives, and every install records who owns its updates so refusals are uniform.

## ADDED Requirements

### Requirement: Release artifacts and targets
Each release SHALL publish one self-contained `prometheus` executable per target — macOS universal (arm64 + x86_64), Linux x86_64 and aarch64 in both glibc and static musl variants, Windows x86_64 and aarch64, and Android/Termux aarch64 — each as an archive with a SHA-256 checksum, a detached signature, an SBOM (SPDX JSON) and a build-provenance attestation, plus a signed `release-manifest.json` listing every artifact's target, size and digest.
**Milestone:** M1

#### Scenario: Manifest covers every artifact
- **GIVEN** a published release `v1.4.0`
- **WHEN** `release-manifest.json` is verified
- **THEN** its signature SHALL verify and it SHALL list a digest for every target archive that matches the downloaded bytes

### Requirement: Release identity grammar
A stable release SHALL be tagged `vMAJOR.MINOR.PATCH` matching `^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$`, and a canary SHALL be `vMAJOR.MINOR.PATCH-canary.YYYYMMDDHHMMSS` (UTC) where MAJOR.MINOR.PATCH is the next planned stable, so that semantic-version ordering places every canary after the previous stable and before its own stable; `prometheus --version` SHALL print the version, channel, target, commit and build date on one line.
**Milestone:** M1

#### Scenario: Canary ordering
- **GIVEN** installed `v1.4.0` and available `v1.5.0-canary.20261029064100`
- **WHEN** versions are compared
- **THEN** the canary SHALL compare greater than `v1.4.0` and less than `v1.5.0`

### Requirement: Signature trust root
The binary SHALL embed the release signing public keys (a primary and a standby key) and SHALL verify the detached signature of every manifest, archive and runtime index it installs against them before use, accepting additionally a keyless transparency-log signature only when `update.require_transparency_log = true` adds that as a second required check; a verification failure SHALL abort with `signature verification failed for <file>` and exit 77, and there SHALL be no flag that skips verification.
**Milestone:** M1

#### Scenario: Tampered archive
- **GIVEN** a downloaded archive whose bytes were modified
- **WHEN** an install or update verifies it
- **THEN** it SHALL fail with `signature verification failed` and the installed binary SHALL be unchanged

### Requirement: Installers
The system SHALL provide `install.sh` (POSIX) and `install.ps1` (Windows) that detect the target, download and verify the matching archive, place the binary at `~/.local/bin/prometheus` (POSIX) or `%LOCALAPPDATA%\Programs\Prometheus\prometheus.exe` (Windows) with a `pmt` alias, add that directory to PATH only with consent (`--modify-path` / `-ModifyPath` for non-interactive runs), and write the install receipt; installers SHALL accept `--channel stable|canary` (default `stable`) and `--version vX.Y.Z`, exiting 2 with `--channel must be stable or canary, not <value>` otherwise.
**Milestone:** M1

#### Scenario: Unknown channel
- **GIVEN** `install.sh --channel main`
- **WHEN** the installer parses arguments
- **THEN** it SHALL exit 2 with `--channel must be stable or canary, not main` before downloading anything

### Requirement: Package-manager and image channels
The system SHALL publish the same signed binaries through a Homebrew tap, a Nix flake, a Termux package, a WinGet manifest and an OCI container image (non-root user, binary at `/usr/local/bin/prometheus`, `PROMETHEUS_HOME=/data`); each of these SHALL stamp its install receipt with `owner` set to the manager (`homebrew`, `nix`, `termux`, `winget`, `image`).
**Milestone:** M2

#### Scenario: Homebrew install is owned by Homebrew
- **GIVEN** `brew install cyberdynecorp/tap/prometheus`
- **WHEN** `prometheus update --status` runs
- **THEN** it SHALL print `owner: homebrew` and `update with: brew upgrade prometheus`

### Requirement: Install receipt
Every installed binary SHALL have an install receipt `install.json` next to it (or, when that directory is not writable, under `<PROMETHEUS_HOME>/updates/install/<sha16 of binary path>.json`) recording `owner` (`self`, a package manager, `image`, `bundle` or `dev`), `channel`, `version`, `target`, `installed_at` and the binary path; a binary with no receipt SHALL be treated as `owner = dev`.
**Milestone:** M1

#### Scenario: Built from source
- **GIVEN** a binary produced by a local source build with no receipt
- **WHEN** `prometheus update --status` runs
- **THEN** it SHALL print `owner: dev` and `this build does not self-update`

### Requirement: Uniform refusal for installs owned elsewhere
`prometheus update` (including `--check` and `--channel`) SHALL refuse every install whose owner is not `self` or `bundle` — package-manager, image and dev builds alike — before any network request or mutation, printing the owner and the command that updates it (or `rebuild from source` for `dev`), writing a `refused` update receipt, and exiting with the single code 69.
**Milestone:** M1

#### Scenario: Same exit code for every foreign owner
- **GIVEN** three installs owned by `homebrew`, `image` and `dev`
- **WHEN** `prometheus update` runs on each
- **THEN** each SHALL exit 69, write a `refused` receipt, and leave the binary unchanged

### Requirement: Update exit codes
`prometheus update` SHALL use only the exit-code table owned by `workspace-architecture`, restated here: 0 when the binary was updated or was already current, 1 when the update failed (after any rollback), 2 on usage errors, 4 when the update needs a user confirmation (for example restarting a running daemon) that cannot be obtained non-interactively, 69 when refused because another owner manages the install, 75 when another update holds the update lock, 77 when a signature check fails, and 10 only for `--check` when a newer version is available.
**Milestone:** M1

#### Scenario: Check finds a newer release
- **GIVEN** installed `v1.4.0` on `stable` and released `v1.4.1`
- **WHEN** `prometheus update --check` runs
- **THEN** it SHALL print `update available: v1.4.0 → v1.4.1` and exit 10

### Requirement: Release channels
The system SHALL support exactly the channels `stable` and `canary`, stored per install in the install receipt (never in a profile's `config.toml`); `prometheus update --set-channel <c>` SHALL persist the channel and exit 0; `--channel <c>` SHALL apply for one run only; and moving from `canary` to `stable` SHALL not downgrade unless `--allow-downgrade` is passed, otherwise staying on the current build until `stable` overtakes it.
**Milestone:** M1

#### Scenario: Leaving canary does not downgrade
- **GIVEN** installed `v1.5.0-canary.20261029064100` and latest stable `v1.4.1`
- **WHEN** `prometheus update --set-channel stable` and then `prometheus update` run
- **THEN** the binary SHALL stay on the canary and print `current build is newer than stable v1.4.1; staying until stable catches up`

### Requirement: Update check without telemetry
The system SHALL check for updates at most once per `update.check_interval_secs` (default 86400, `0` disables) by fetching the channel's signed index with a request that carries only the current version and target in the user agent and no install id, profile, usage or host identifiers; a found update SHALL be shown as a one-line notice in the CLI and TUI and SHALL never be applied automatically unless `update.auto_apply = true` (default false).
**Milestone:** M1

#### Scenario: Checks disabled
- **GIVEN** `update.check_interval_secs = 0`
- **WHEN** any command runs
- **THEN** no request to the release server SHALL be made

### Requirement: Single-writer update lock
`prometheus update` SHALL take an exclusive lock at `<PROMETHEUS_HOME>/updates/update.lock` keyed to the binary path before any mutation, and a second updater SHALL print `another update is running (pid <pid>, started <time>)` and exit 75 without waiting.
**Milestone:** M1

#### Scenario: Concurrent updates
- **GIVEN** an update in progress
- **WHEN** a second `prometheus update` starts
- **THEN** it SHALL exit 75 and change nothing

### Requirement: Atomic binary swap with health check
An update SHALL download and verify the new archive into `<PROMETHEUS_HOME>/updates/staging/`, run the staged binary's `self-test` (version, embedded schema versions, sandbox probe, opening each profile's config read-only) with a 30 s timeout, and only on success atomically replace the installed binary (rename on POSIX; rename-aside of the running executable on Windows), keeping the previous binary as `prometheus.prev` beside it; a failed self-test SHALL leave the installed binary untouched and exit 1 with `staged build failed self-test: <reason>`.
**Milestone:** M1

#### Scenario: Broken build never goes live
- **GIVEN** a staged binary whose self-test exits non-zero
- **WHEN** the update proceeds
- **THEN** the installed binary SHALL be unchanged, the receipt outcome SHALL be `failed` and the command SHALL exit 1

#### Scenario: Power loss during swap
- **GIVEN** the host loses power during the swap
- **WHEN** it boots
- **THEN** the binary path SHALL hold either the complete old or the complete new binary

### Requirement: Rollback
`prometheus update --rollback` SHALL restore `prometheus.prev` atomically and record a receipt with outcome `rolled_back`; it SHALL refuse with exit 1 and `cannot roll back: profile <p> state.db schema <n> is newer than <prev version> supports (max <m>); rerun with --restore-snapshots to restore the pre-update snapshot` when any profile's storage was migrated beyond what the previous binary reads, and with `--restore-snapshots` SHALL restore the pre-migration snapshots taken by that update and print how many sessions created since would be lost before asking for confirmation.
**Milestone:** M1

#### Scenario: Rollback after compatible update
- **GIVEN** an update from `v1.4.0` to `v1.4.1` with no schema change
- **WHEN** `prometheus update --rollback` runs
- **THEN** `prometheus --version` SHALL report `v1.4.0` and the receipt SHALL say `rolled_back`

### Requirement: Schema migrations around updates
A new binary SHALL migrate each profile's `config.toml` and `state.db` forward on first open, taking a snapshot of both into `<profile>/snapshots/pre-<version>/` before migrating, keeping the newest `update.snapshot_keep` snapshots (default 3); every binary SHALL declare the maximum storage schema it can read and SHALL refuse to open a newer `state.db` with `state.db schema <n> is newer than this build supports (<m>); update prometheus or roll back with --restore-snapshots`.
**Milestone:** M1

#### Scenario: Older binary meets newer schema
- **GIVEN** a profile migrated by `v1.6.0` and a manually started `v1.5.0` binary
- **WHEN** it opens that profile
- **THEN** it SHALL exit 1 with the `newer than this build supports` message and SHALL NOT modify the database

### Requirement: Running daemon handover
After a successful swap the update SHALL ask a running gateway daemon of the same binary path to drain (finish in-flight turns up to `update.drain_timeout_secs`, default 120) and re-exec the new binary, reporting `daemon restarted on <version>`; when `--no-restart` is passed or the drain times out, the update SHALL keep outcome `success`, record follow-up `daemon_restart_pending` and print the command to restart it.
**Milestone:** M2

#### Scenario: Busy daemon
- **GIVEN** a daemon with a turn still running after 120 s of drain
- **WHEN** the update finishes
- **THEN** the receipt SHALL list follow-up `daemon_restart_pending` and the command SHALL exit 0

### Requirement: Update receipts at the root home
Every update, check, refusal and rollback SHALL write a receipt under `<PROMETHEUS_HOME>/updates/receipts/` (the root home, never a profile directory) as `<timestamp>-<id>.json` plus `latest.json`, with `outcome` in `running`, `success`, `failed`, `refused`, `rolled_back` or `interrupted`, the from/to versions, channel, stages completed, follow-ups and errors; a `running` receipt whose process is gone SHALL be marked `interrupted` by the next update, and only the newest 50 receipts SHALL be kept.
**Milestone:** M1

#### Scenario: Profile does not change receipt location
- **GIVEN** a command run with `--profile work`
- **WHEN** `prometheus update` runs
- **THEN** its receipt SHALL be written to `<PROMETHEUS_HOME>/updates/receipts/`, not inside the `work` profile

### Requirement: Update dry-run and changelog preview (upgrade)
`prometheus update --dry-run` SHALL resolve the target version, verify the signed manifest and print, without downloading the binary or mutating anything, the from/to versions, download size, the changelog entries between the two versions grouped as Breaking, Features, Fixes and Security (from the signed release notes in the manifest), the config and storage migrations that would run per profile, the managed runtimes whose pins would change, and whether a daemon restart is needed; `--json` SHALL emit the same as one object, and `--dry-run` SHALL exit 0 or 10 like `--check`.
**Milestone:** M2

#### Scenario: Preview a breaking update
- **GIVEN** installed `v1.4.1` and stable `v2.0.0` whose notes mark a breaking config change
- **WHEN** `prometheus update --dry-run` runs
- **THEN** the output SHALL list the Breaking entry, name the config migration `v1→v2` for each profile, and the installed binary and homes SHALL be unchanged

### Requirement: Offline bundles (upgrade)
`prometheus bundle create --target <t> [--version v] [--with node,cpython,chromium] -o <file>` SHALL produce a single archive containing the signed release manifest, the target's binary archive, the selected runtime archives with their signed runtime index, and a bundle manifest; `install.sh --bundle <file>`, `install.ps1 -Bundle <file>`, `prometheus update --from-bundle <file>` and `prometheus runtimes install --from-bundle <file>` SHALL install from it with no network access, verifying every signature against the embedded keys, and SHALL stamp the install `owner = bundle`.
**Milestone:** M2

#### Scenario: Air-gapped update
- **GIVEN** a host with no network and a bundle for `v1.4.1` built on another machine
- **WHEN** `prometheus update --from-bundle prometheus-v1.4.1-linux-x86_64-musl.bundle` runs
- **THEN** the update SHALL complete with no network request and the receipt SHALL record `source: bundle`

#### Scenario: Bundle for another target
- **GIVEN** a bundle built for `aarch64-apple-darwin` on a Linux x86_64 host
- **WHEN** it is used for update
- **THEN** the command SHALL exit 1 with `bundle target aarch64-apple-darwin does not match this host (x86_64-unknown-linux-musl)`

### Requirement: Managed runtime store
The system SHALL install external runtimes (`node`, `cpython`, `uv`, `chromium`) into a tool store shared by all profiles at `<PROMETHEUS_HOME>/runtimes/<name>/<version>-<sha12>/`, from a runtime index pinned by the binary that records per target the URL, size and SHA-256 of each artifact; a download whose digest differs SHALL be deleted and fail with `sha256 mismatch for <url>: pinned <expected>, got <actual>`, and a runtime directory SHALL become visible only by an atomic rename after verification and extraction.
**Milestone:** M1

#### Scenario: Interrupted download leaves no half runtime
- **GIVEN** a Chromium download killed at 60%
- **WHEN** the browser tool next needs Chromium
- **THEN** no partial directory SHALL be used and the download SHALL restart or resume and be verified

### Requirement: Lazy runtime installation policy
The system SHALL install a missing pinned runtime on first use (for example `chromium` when a browser tool is first called) when `runtimes.allow_lazy_install = true` (default), prompting `Install <name> <version> (<size>)? [Y/n]` on an interactive TTY and installing without prompting in non-interactive sessions; when it is `false`, the tool SHALL fail with `<name> is not installed and lazy installs are disabled: run 'prometheus runtimes install <name>'` and the tool's availability check SHALL report it unavailable.
**Milestone:** M1

#### Scenario: Lazy install disabled in an image
- **GIVEN** the container image, which ships `runtimes.allow_lazy_install = false` in its default config
- **WHEN** a session calls the browser tool without Chromium in the store
- **THEN** the tool SHALL fail with the `lazy installs are disabled` message and nothing SHALL be downloaded

### Requirement: Runtime CLI
`prometheus runtimes` SHALL provide `list` (installed and pinned versions, size, users), `install <name>... [--from-bundle f]`, `remove <name>`, `verify` (re-hash every installed file against the recorded digests and exit 1 on any mismatch, printing `✗ <name>: realized bytes do not match recorded digest`), `gc` and `doctor`, and `prometheus runtimes install` with an unknown name SHALL exit 2 with `unknown runtime '<n>' (expected node|cpython|uv|chromium)`.
**Milestone:** M1

#### Scenario: Corrupted runtime detected
- **GIVEN** a modified file inside the installed Node runtime
- **WHEN** `prometheus runtimes verify` runs
- **THEN** it SHALL report the Node mismatch and exit 1

### Requirement: Runtime leases and garbage collection
Every process using a runtime SHALL hold a shared lease on its version directory, and `prometheus runtimes gc` (also run after each successful update) SHALL remove only versions that are neither pinned by the installed or `.prev` binary nor leased, plus partial downloads older than 24 h, printing the bytes reclaimed.
**Milestone:** M1

#### Scenario: In-use runtime survives gc
- **GIVEN** a Python bridge process running on an unpinned older CPython
- **WHEN** `prometheus runtimes gc` runs
- **THEN** that CPython directory SHALL be kept until the process exits

### Requirement: Download retry budget
Release and runtime downloads SHALL retry only transient failures (HTTP 408, 429, 500, 502, 503, 504, connection resets, timeouts, temporary DNS failures) with exponential backoff capped at 30 s and at most 4 consecutive failures without byte progress, resuming with range requests when the server supports them, and SHALL honour `network.proxy` and the system CA store plus `network.extra_ca_file`.
**Milestone:** M1

#### Scenario: Flaky mirror
- **GIVEN** a download that fails twice with HTTP 503 and then succeeds
- **WHEN** the runtime installs
- **THEN** it SHALL complete and the receipt SHALL record 2 retries

### Requirement: Doctor reports distribution health
`prometheus doctor` SHALL report the install owner, channel, version, signature-key fingerprints, last update receipt, pending follow-ups, the managed runtimes with verification state, and the OS sandbox enforcement available on this host, and SHALL exit 1 when the last update receipt is `failed` or `interrupted` or any runtime fails verification.
**Milestone:** M1

#### Scenario: Interrupted update surfaced
- **GIVEN** a last receipt marked `interrupted` at stage `swap`
- **WHEN** `prometheus doctor` runs
- **THEN** it SHALL print `last update interrupted at stage swap` with the remedy and exit 1

### Requirement: Reproducible, locked builds
Release builds SHALL compile with the committed dependency lockfile in locked mode, from a tagged commit in a clean tree, with a dependency audit (known vulnerabilities, license allow-list, banned sources) that fails the release on any finding not explicitly waived with a reason and expiry date in the repository; the embedded runtime index SHALL be generated from the same commit and its digest printed by `prometheus --version --verbose`.
**Milestone:** M1

#### Scenario: Unwaived advisory blocks the release
- **GIVEN** a dependency with a published security advisory and no waiver
- **WHEN** the release pipeline runs
- **THEN** no artifact SHALL be published

### Requirement: Uninstall
`prometheus uninstall` SHALL remove the binary, its alias and `prometheus.prev` for `owner = self` or `bundle` installs, stop a running daemon of that binary first, keep `PROMETHEUS_HOME` unless `--purge` is given (which lists what will be deleted and asks for confirmation, or requires `--yes` non-interactively), and for installs owned elsewhere SHALL exit 69 naming the manager's uninstall command.
**Milestone:** M2

#### Scenario: Data kept by default
- **GIVEN** a self-installed binary
- **WHEN** `prometheus uninstall` runs
- **THEN** the binary SHALL be gone and `<PROMETHEUS_HOME>` SHALL still exist

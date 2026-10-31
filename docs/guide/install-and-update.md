# Installing and updating

Prometheus is one signed executable per platform. The installers download
it from the release origin, verify it against the release keys built into
the installer, and record who owns its updates; `prometheus update` keeps
it current the same way. External runtimes the tools need (Node, CPython,
uv, Chromium) are installed separately, on demand, into a store shared by
every profile.

Every command on this page runs in CI: the `console` examples against a
fresh home in `scripts/check`, the installer and update commands against a
local signed release (`scripts/ci/test-install-guide` on Linux,
`scripts/ci/test-install-guide.ps1` on Windows).

## Installing

On macOS, Linux and Termux:

```sh
curl -fsSL https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download/install.sh | sh
```

The script installs `~/.local/bin/prometheus` and the `pmt` alias, writes
the install receipt `~/.local/bin/install.json` (owner `self`, channel
`stable`) and offers to add `~/.local/bin` to `PATH`. Options go after
`sh -s --`:

- `--channel stable|canary`: the release channel (kept in the receipt);
- `--version vX.Y.Z`: a specific release;
- `--no-modify-path` / `--modify-path`: do not ask about `PATH`;
- `--base-url URL`: a mirror of the release origin (still verified);
- `--without RUNTIME` (repeatable): never install that runtime (see
  [Opting out of runtimes](#opting-out-of-runtimes)).

```sh
curl -fsSL https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download/install.sh | sh -s -- --no-modify-path --without chromium
```

On Windows (PowerShell 5.1 or 7):

```powershell
irm https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download/install.ps1 | iex
```

This installs `%LOCALAPPDATA%\Programs\Prometheus\prometheus.exe` with
`pmt.exe`. The same options exist as parameters (`-Channel`, `-Version`,
`-NoModifyPath`, `-BaseUrl`, `-Without chromium,node`); to pass them, run
the downloaded script as a script block:

```powershell
& ([scriptblock]::Create((irm https://github.com/CyberdyneCorp/prometheus-agent/releases/latest/download/install.ps1))) -NoModifyPath -Without chromium
```

Both installers verify the minisign signatures of the channel index and
release manifest and the archive's SHA-256 before installing anything; a
tampered download exits 77 with `signature verification failed for
<file>` and leaves an existing install alone. Installs made by a package
manager (Homebrew, Nix, WinGet, ...), the container image or a source
build are updated by that owner instead: `prometheus update` refuses them
with exit 69 and names the right command.

Check what was installed and who updates it:

```sh
prometheus --version
prometheus doctor distribution
```

## Updating

```sh
prometheus update --check
prometheus update
prometheus update --status
```

`update --check` only asks: it exits 10 when a newer release is
available and 0 when this build is current. `prometheus update` holds an
update lock (a second one exits 75), downloads and verifies the release
for this platform, runs the new build's self-test, takes the pre-update
backup, swaps the binary atomically and keeps the previous one as
`prometheus.prev`. Every run leaves a receipt under
`<home>/updates/receipts/`; `update --status` and `prometheus doctor
distribution` show the last one, and the doctor fails when the last
update failed or was interrupted (`last update interrupted at stage
swap`, with the remedy).

The pre-update backup follows `update.pre_update_backup` in
`<home>/daemon.toml`: `state` (default) snapshots each profile's
config, grants and `state.db` (listed by `prometheus snapshots list`);
`full` also writes a whole-home archive
`updates/backups/pre-update-<from>-<to>.tar.zst` (the newest two are
kept); `off` skips it, except for the snapshot a store migration needs.
`--backup` forces `full` and `--no-backup` forces `off` for one run. A
backup that cannot be written (a full disk) stops the update before the
swap with exit 73.

```sh
prometheus update --backup
prometheus update --rollback
```

`--rollback` puts `prometheus.prev` back. When the newer build already
migrated a profile's store beyond what the previous build reads, the
rollback refuses and `--rollback --restore-snapshots` restores the
pre-update snapshots instead (losing what changed since, after asking).

Channels: `--channel canary` checks or installs from the canary channel
once; `--set-channel canary` makes it the install's channel. Leaving
canary never downgrades: `--allow-downgrade` installs the stable release
anyway. Background checks (`update.check_interval_secs`, daily by
default, `0` disables them) send only the version and platform.

Mirrors: `update.mirror_url` serves release files and
`runtimes.mirrors.runtimes` runtime archives in place of the default
origins. Mirrored files are verified exactly like the originals, so a
mirror cannot change what is installed; with `update.mirror_fallback =
true` a failing mirror falls back to the default origin. Downloads retry
transient failures (HTTP 408, 429, 5xx gateways, resets, timeouts) with
backoff up to 30 s, resume partial files, and give up after four
failures in a row without progress. They use `network.proxy` and
`network.extra_ca_file` of the selected profile.

## Managed runtimes

Runtimes live in `<home>/runtimes/<name>/<version>-<sha12>/`. Each
binary pins one version of each, with the URL, size and SHA-256 of the
archive for every platform; a download whose digest differs is deleted
and fails with `sha256 mismatch for <url>: pinned <expected>, got
<actual>`. A tool that needs a missing runtime installs it on first use
(asking `Install <name> <version> (<size>)? [Y/n]` on a terminal) unless
`runtimes.allow_lazy_install = false`, in which case it fails with a
`prometheus runtimes install <name>` hint.

```console
$ prometheus runtimes list
node <version> pinned
not installed
$ prometheus runtimes install java # exits 2
error: unknown runtime 'java' (expected node|cpython|uv|chromium)
$ prometheus runtimes verify
no managed runtimes installed
$ prometheus runtimes gc
reclaimed 0 bytes
```

```sh
prometheus runtimes install uv
prometheus runtimes verify
```

`verify` re-hashes every installed file against the digests recorded at
install time and exits 1 on any difference (`✗ <name>: realized bytes do
not match recorded digest`); `prometheus doctor runtimes` reports the
same. A process using a runtime holds a lease on its version, and
`runtimes gc` (also run after every update) removes only versions that
neither the installed binary nor `prometheus.prev` pins and that no
process uses, plus partial downloads older than a day. `runtimes remove
<name>` deletes the versions not in use.

### Opting out of runtimes

`runtimes.exclude` in `daemon.toml` lists runtimes nothing may install:
not `runtimes install`, not a tool's lazy install, not the refresh after
an update. `install.sh --without <name>` and `install.ps1 -Without
<name>` record names there; `prometheus runtimes exclude` edits it.

```console
$ prometheus runtimes exclude add chromium
runtimes.exclude = [chromium]
$ prometheus runtimes exclude list
chromium
$ prometheus runtimes install chromium # exits 1
error: chromium is excluded by runtimes.exclude: run 'prometheus runtimes exclude remove chromium' to allow it
$ prometheus runtimes exclude remove chromium
runtimes.exclude = []
```

## Building from source

A source build (`cargo build --locked -p prometheus`) has no install
receipt: it reports owner `dev` and never updates itself. Release builds
compile with the committed `Cargo.lock` in locked mode and the commit
time as build date, so rebuilding a commit reproduces its binary (CI
rebuilds the Linux musl target twice and compares the digests);
`prometheus --version --verbose` prints the digest of the runtime index
the build pins. Dependency changes pass two gates in `scripts/check` and
the release pipeline: the advisory, license and source audit
(`cargo deny`), and a release-age gate that rejects crate versions
published less than `release_age_days` (3) before they were locked,
unless `deps-policy.toml` waives them with a reason and an expiry date.

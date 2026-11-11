# Package managers, containers and offline installs

Besides `install.sh` and `install.ps1` ([Installing and updating](install-and-update.md)),
every release ships the same signed binaries through package managers, a
container image, a Nix flake and offline bundles. Each install records who
updates it in its receipt (`install.json` beside the binary), and
`prometheus update` refuses (exit 69) any install it does not own, naming
the command that does:

| Channel | Install | Receipt owner | Update with |
|---|---|---|---|
| Homebrew | `brew install cyberdynecorp/tap/prometheus` | `homebrew` | `brew upgrade prometheus` |
| WinGet | `winget install CyberdyneCorp.Prometheus` | `winget` | `winget upgrade CyberdyneCorp.Prometheus` |
| Termux | `pkg install prometheus` | `termux` | `pkg upgrade prometheus` |
| Nix | `nix profile install github:CyberdyneCorp/prometheus-agent/flake` | `nix` | your Nix configuration |
| Container | `docker pull ghcr.io/cyberdynecorp/prometheus` | `image` | pull the new image |
| Offline bundle | `install.sh --bundle FILE` | `bundle` | `prometheus update --from-bundle FILE` |

A WinGet portable install runs no scripts, so it has no receipt:
Prometheus recognises the WinGet packages directory it runs from. A source
build has no receipt either and says so:

```console
$ prometheus update --status
owner: dev
this build does not self-update
```

## Updating: preview, daemon handover, rollback

`prometheus update --dry-run` verifies the target release's signed
manifest and prints, without downloading the binary or changing anything,
the versions and download size, the changelog entries in between grouped as
Breaking, Features, Fixes and Security, the configuration and storage
migrations each profile would run (`config v1→v2`), the managed runtimes
whose pins change, and whether a running daemon restarts. It exits 10 when
an update is available, like `--check`; `--json` prints the same as one
object.

After the binary is swapped, a running gateway daemon started from that
binary finishes its turns (up to `update.drain_timeout_secs` in
`daemon.toml`, default 120) and restarts on the new build: under its
supervisor (systemd, launchd, the container's init) or started again by the
update, which then prints `daemon restarted on <version>`. Turns still
running at the timeout are never cut short: the daemon keeps the old build,
the update succeeds, and its receipt lists `daemon_restart_pending` with the
command to run later (`prometheus daemon restart`). `--no-restart` leaves the
daemon alone the same way. Restarting a daemon whose turns are running is
asked on a terminal; without one it needs `--yes`, else the update stops
before the swap (exit 4).

### From a chat

With `update.allow_from_chat = true` in `daemon.toml` (off by default),
gateway admins can send `/update` (and `/update check`) from a chat. After
a confirmation in the same chat, the same updater runs detached; its stages
(`checking`, `downloading <n>%`, `verifying`, `self-test`, `swapping`,
`restarting`) arrive at most once every 2 s, refusals (another owner, the
update lock, a bad signature) arrive with their message, a busy daemon's
restart is asked in the chat (no answer within `approvals.timeout_secs` is
a no), and the restarted daemon says `updated to <version>`.

## Offline bundles

An air-gapped host installs and updates from one file built elsewhere:

```text
prometheus bundle create --target x86_64-unknown-linux-musl --with node,chromium -o prometheus.bundle
sh install.sh --bundle prometheus.bundle            # a new install (install.ps1 -Bundle on Windows)
prometheus update --from-bundle prometheus.bundle   # an existing self or bundle install
prometheus runtimes install --from-bundle prometheus.bundle
```

A bundle holds the signed release manifest, the target's archive, the
signed runtime index and the selected runtime archives. Nothing is
downloaded: every signature is still checked against the keys embedded in
the binary, and every runtime archive against the installing binary's own
pinned digest. A bundle for another target is refused (`bundle target
aarch64-apple-darwin does not match this host (x86_64-unknown-linux-musl)`,
exit 1). The install's receipt says `owner = bundle` and update receipts
record `source: bundle`.

## Container image

`ghcr.io/cyberdynecorp/prometheus:<version>` (linux/amd64 and linux/arm64)
holds the binary, CA certificates and the PulseAudio client tools;
`<version>-full` adds verified Node, CPython, uv and Chromium, copied into
`/data/runtimes/` on first start. The image runs as the non-root user
`prometheus` (uid and gid 10001), keeps everything in its only volume
`/data` (`PROMETHEUS_HOME`), works with `--read-only` plus a tmpfs `/tmp`,
publishes no port, and checks its health with `prometheus status
--liveness`. `prometheus` is PID 1: it reaps orphaned processes and passes
`docker stop`'s SIGTERM to the daemon.

```text
docker run -d --name prometheus --read-only --tmpfs /tmp -v prometheus-data:/data ghcr.io/cyberdynecorp/prometheus
docker exec -it prometheus prometheus status        # as uid 10001, on the same /data
docker exec -it prometheus prometheus chat
docker exec prometheus prometheus profile create work
docker exec prometheus prometheus daemon restart    # in place: the container keeps running
```

The one daemon serves every profile under `/data/profiles/`; listeners
(`gateway.http` and the others in `daemon.toml`) are daemon-wide and
address profiles by path, so one published port serves them all. A
listener whose port is taken is reported `down` with `port <n> already in
use` while everything else keeps running.

Each release attaches a reference `compose.yaml` (named volume,
`restart: unless-stopped`, the health check, `mem_limit: 2g`,
`pids_limit: 512`, `host.docker.internal` for servers on the host) with an
opt-in `audio` profile that mounts the host's PulseAudio or PipeWire-Pulse
socket at `/run/pulse/native` for voice mode. Inside a container a provider
on `localhost` is the container itself: `prometheus doctor providers`
warns and suggests `http://host.docker.internal:<port>`.

### Network-isolated deployment

`compose.isolated.yaml` puts Prometheus on an `internal: true` network whose
only way out is an egress proxy that allows just the hosts the profiles
need. Generate the allowlist from the configuration, point each profile at
the proxy and declare the isolation:

```text
prometheus doctor network --allowlist > allowlist.txt
prometheus config set network.proxy http://egress-proxy:3128
prometheus config set network.expect_isolated true
prometheus doctor network --online
```

The doctor then reports `direct egress: blocked` and each provider host as
reachable through the proxy; attached to a bridge network by mistake, it
fails with `direct egress succeeded although network.expect_isolated =
true` (exit 1). Without `--online` nothing touches the network:

```console
$ prometheus doctor network --allowlist
github.com
```

## NixOS and Home Manager

The flake exports `nixosModules.default` (`services.prometheus-agent`: nixpkgs already has
`services.prometheus` for the monitoring server) and
`homeManagerModules.default` (`programs.prometheus`) with `enable`,
`package`, `home`, `settings` (the default profile), `profiles.<name>.settings`,
`daemonSettings` (`daemon.toml`), `environmentFiles` (secrets only, as
`NAME=value` lines; sops-nix and agenix outputs work), `documents` (such as
`SOUL.md`), `mcpServers`, `plugins` (linked read-only; signatures and grants
still apply) and `container.enable` (run the image instead).

```text
services.prometheus-agent = {
  enable = true;
  settings.model.default = "claude-sonnet-4-5";
  environmentFiles = [ config.sops.secrets.prometheus.path ];
};
```

The NixOS module writes `settings` into the managed configuration layer
(`/etc/prometheus/managed.toml`) with every key locked, so `prometheus
config set model.default gpt-5` refuses (exit 77). Home Manager's default
`configMode = "managed"` regenerates each declared profile's `config.toml`
on activation and writers refuse naming the option; `"merged"` rewrites
only the Nix-set keys and keeps your other edits. Both run `prometheus
daemon run` as a service (systemd, or a launchd agent on macOS) that
restarts on failure but never on exit codes 2, 65, 77 and 78, so a bad
configuration stops with status 78 instead of restart-looping.

## Uninstalling

`prometheus uninstall` stops the daemon, removes the service units `daemon
install-service` created, then the binary, its `pmt` alias,
`prometheus.prev` and the receipt; the home stays. `--purge` also deletes
the home, `--data-only` deletes the home's contents and keeps the binary,
`--desktop-data` deletes the desktop app's per-user data, and `--dry-run`
lists every file, directory, unit and process it would remove or stop.
Deleting data asks first (`--yes` without a terminal, else exit 4). A
binary installed by a package manager, Nix, Termux, the image or a desktop
package is removed by its owner (exit 69 naming how), while `--data-only`
and `--desktop-data` still work:

```console
$ prometheus uninstall --dry-run # exits 69
$ prometheus uninstall --data-only --dry-run
nothing was changed (dry run)
```

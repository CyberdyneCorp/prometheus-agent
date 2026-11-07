# OS sandbox and terminal backends

Every local child process (terminal commands, background processes, code
execution, MCP stdio servers, hook and skill scripts) runs inside an OS
sandbox derived from the session's capability set. The sandbox is the
boundary; approvals and scanners are heuristics on top of it.

| OS | Mechanism | `fs` | `net` | `process` | `syscalls` |
|---|---|---|---|---|---|
| Linux | Landlock + seccomp | enforced (Landlock) | enforced (Landlock TCP rules, ABI 4 / Linux 6.7, plus seccomp for non-TCP IP sockets) | enforced (seccomp; signal scoping on Linux 6.12+) | enforced (seccomp) |
| macOS | Seatbelt (`sandbox-exec`) | enforced | enforced | enforced | partial (platform maximum) |
| Windows | AppContainer + Job Object | enforced (ACL entries for the container SID) | enforced (`internetClient` capability, none, or only the egress proxy through a relay in the container) | enforced (Job Object) | partial (platform maximum) |

`partial` at the platform maximum is not a downgrade. Other platforms
(Android, the BSDs) have no backend: every feature is `unavailable`.

On Windows the container may also list and read the attributes of the
directories above its granted roots (names and attributes, never contents,
like `file-read-metadata` on macOS): PowerShell checks every component of a
location before entering it. A directory whose permissions your account may
not change (for example `C:\Users` without elevation) is skipped with a
warning, and confined PowerShell may then be unable to enter locations
below it.

## What a child may touch

The built-in baseline (used when `security.capabilities` is unset) is:

```toml
[security]
capabilities = [
  "fs.read:<workspace>/**", "fs.write:<workspace>/**",
  "fs.read:<scratch>/**", "fs.write:<scratch>/**",
  "fs.read:<toolchain>",   # /usr, /bin, /etc, /System, ... per OS
  "process.spawn:*", "net.connect:*", "llm.call",
]
```

- Path globs are resolved against canonical paths (symlinks resolved). The
  OS sandbox can express a literal path or a trailing `/**`; any other
  wildcard is dropped, so the sandbox is narrower than the grant, never wider.
- Credential stores are denied even under a broader grant such as
  `fs.read:~/**`: `PROMETHEUS_HOME` (except the session's own scratch and
  spill directories), `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.config/gcloud`,
  `~/.azure`, `~/.kube`, `~/.docker/config.json`, `~/.netrc`, OS keychain
  files and browser profile directories. A grant that names a path inside a
  store (for example `fs.read:~/.aws/config`) is honoured.
- Without `process.spawn`, a child cannot start further processes.
- The OS filters ports, not host names, so a session's children reach the
  network only through its loopback egress proxy, which filters hosts (see
  [Network egress](security.md#network-egress)). On macOS the Seatbelt
  profile admits the proxy's port on loopback alone. On Linux Landlock
  filters the port, not the address, so a proxied child runs in a network
  namespace of its own whose only way out is a listener on the proxy's
  port, served by the proxy; this needs unprivileged user namespaces
  (Ubuntu 24.04 restricts them through AppArmor:
  `sysctl kernel.apparmor_restrict_unprivileged_userns=0`). Without them
  `net` is `partial` (`landlock_net`) and the downgrade rule applies. On
  Windows an AppContainer reaches a loopback listener only of its own
  container, so the launcher starts a small relay process (the same
  executable) inside the child's container: the child gets no network
  capability at all, its proxy variables point at the relay's loopback
  port, and the relay forwards each connection over a named pipe (open
  only to your account and that container) to the launcher, which
  connects it to the proxy. Direct connections, to the proxy's own port,
  other loopback services, the private network or the internet, fail, so
  `net` is `enforced` (mechanism `appcontainer relay`). A child given its own
  `net.connect:<host>` grant without the proxy reports `net` `partial`. UDP
  sockets are refused only when outbound connections are restricted. `/proc` is not
  readable on Linux (other processes' environments hold secrets).

Known limits: on Linux and Windows the sandbox can only grant, so a granted
directory that contains a credential store is granted entry by entry: files
created directly in it after the child starts are not reachable, and the
store's name stays listable. Symbolic links in such a directory are not
granted (a link such as `~/.ssh -> ~/dotfiles/ssh` would otherwise expose
its target); a link whose target lies in a granted directory still works. On
Windows, listening sockets are never granted (loopback needs a
per-container exemption), and the session adds then revokes ACL entries for
its container on the granted directories (and listing entries on the
directories above them). Sessions that start or end at the same time take
turns on these ACL changes (a named mutex of the logon session), so one
session's change never drops another's entry. Below a directory whose ACL is not
marked auto-inherited (uncommon; seen on some CI images), Windows keeps the
other entries' rights but stores them as explicit entries after the
revocation, so later permission changes on the parent no longer flow down.

## Downgrades are never silent

When the host cannot enforce a feature, commands are refused:

```text
sandbox unavailable: landlock not supported by kernel; set security.sandbox.allow_degraded = ["landlock"] to run without it
```

To run anyway, name every missing feature (`landlock`, `landlock_net`,
`seccomp`, `seatbelt`, `appcontainer`, or `all`):

```toml
[security.sandbox]
allow_degraded = ["landlock_net"]
```

Every start then logs a WARN naming the feature, the session banner shows
the level, for example `degraded: sandbox: partial:landlock_net`, and each
session records one `security.downgrade` trajectory event and one audit
record (`kind = "downgrade"`) per missing feature. Local browser switches
that turn off Chromium's sandbox or web security (`chromium_sandbox`,
`web_security`) are reported the same way (see the browser guide).
`security.sandbox.mode = "off"` disables the sandbox and needs
`allow_degraded = ["all"]`.

## Terminal backends

`terminal.backend` selects where commands run: `local` (the sandboxed
host), `container` (a hardened rootless container), `ssh` (another host),
or a backend registered by an enabled `terminal-backend` plugin. Any other
name fails validation (exit 78) and lists the available backends.
`prometheus chat` (and every session start) checks the setting before the
first turn, with the same exits; Prometheus never falls back to another
backend: a backend that cannot be set up (no runtime, no SSH destination)
refuses every command with the reason. Defaults: `timeout_secs = 180`,
`max_foreground_secs = 600`, `idle_lifetime_secs = 300`,
`persistent_shell = true`; container and SSH settings are only type-checked
while another backend is selected. `prometheus doctor terminal` shows the
selected backend's readiness (exit 0 ready, 1 not).

Every backend declares an isolation descriptor (`isolation` = `os_sandbox`,
`container` or `remote`, `uses_host_filesystem`, `default_cwd`,
`session_isolated_when_ephemeral`, `persistent`); approval and file-tool
decisions read only the descriptor. On `container`, `ssh` and plugin
backends the file tools act inside that environment: `read_file
/etc/hostname` reads the container's file, not the host's.

### Container backend

`terminal.container.runtime` is `auto` (Podman, else Docker), `podman`,
`docker` or the path of either CLI. The runtime must be rootless (Docker
Desktop, whose daemon runs in its own VM, counts as rootless); a rootful
daemon needs `security.sandbox.allow_degraded = ["all"]`. Every container
starts with `--cap-drop ALL` plus only `CHOWN`, `DAC_OVERRIDE` and `FOWNER`,
`--security-opt no-new-privileges`, a read-only root, tmpfs `/tmp`
(`rw,exec,nosuid,size=512m`: Docker would mount it `noexec` otherwise) and `/var/tmp` (`rw,noexec,nosuid,size=256m`), the
configured pids, CPU and memory limits, and the labels
`prometheus.profile=<profile>` and `prometheus.session=<session>` (plus
`prometheus.persistent=true` on a persistent container). A flag the
runtime rejects fails creation with `container hardening '<flag>'
unsupported by runtime` unless `allow_degraded` names its feature
(`cap_drop`, `no_new_privileges`, `read_only_rootfs`; `all` for the tmpfs
mounts and the limits), in which case it is dropped with a WARN on every
start.

Each container also records how it was made: `prometheus.spec` is a digest
of its `create` settings (image, volumes, network, user, limits, extra
arguments, environment) and `prometheus.dropped` lists the flags dropped
under `allow_degraded`. An existing container is reused only when its
digest matches the current settings and every dropped flag is still
waived; reusing it repeats the WARN for each dropped flag, in any process.
Otherwise it is removed and created again, so a later configuration change
(or a removed `allow_degraded` entry) always takes effect, and a flag the
runtime still rejects then fails creation naming it.

| Setting | Default | Meaning |
|---|---|---|
| `image` | the release's `prometheus-sandbox` image | the image |
| `mount_workspace` | `true` | the session workspace at `/workspace` (the start directory) |
| `volumes` | `[]` | `host:container[:ro]`; `PROMETHEUS_HOME`, credential stores and runtime sockets are refused |
| `network` | `true` | `false` runs the container with no network |
| `run_as_host_user` | `false` | `--user <uid>:<gid>` |
| `shared_key` | `""` | sessions of the profile with the same key share one container |
| `persistent` | `true` | one container per profile and workspace (per profile without a workspace mount), reused by later sessions and stopped, never removed; `false` gives each session its own container, removed at session end |
| `extra_args` | `[]` | extra `create` arguments; `--privileged`, `--cap-add`, a weakening `--security-opt`, `--pid/--network/--userns=host` and runtime-socket mounts are refused unless `allow_degraded` covers them |

A container idle longer than `idle_lifetime_secs` is torn down (stopped
when persistent, removed otherwise) unless a foreground call is in flight or
a background process runs; the next command starts it again. Once per
gateway start, exited non-persistent containers of the profile that
finished more than twice the idle lifetime ago are removed; other profiles'
containers and stopped persistent ones are never touched. The root
filesystem is read-only and `/tmp` is a tmpfs, so what a persistent
container keeps across a stop is the container itself and its volumes; use
the workspace or a volume for files that must outlive it. A runtime that stops answering turns the next command into a
`degraded` result and the call after it recreates the environment.

### SSH backend

`terminal.ssh.host` and `terminal.ssh.user` are required (`port` defaults to
22; `config_file` is passed to `ssh -F` for identities, known hosts or jump
hosts). Commands run over one multiplexed connection in batch mode (never a
password prompt) and start in the remote home. Granted secrets never appear
on a command line: they are written to a private file the command sources
and deletes. With `terminal.sync_back` (default on for `ssh`), files changed
under `terminal.ssh.workspace` are copied back to the host workspace at
session end and on `/sync`, up to `terminal.sync_back_max_bytes` (2 GiB) per
sync; files over the budget are skipped and listed, and a host file edited
since the previous sync is reported as a conflict, never overwritten.

On every backend that does not use the host filesystem (container, SSH,
plugin), a command that names a file of the profile's received attachments
(`cache/media/`) or of a skill directory on this host gets that input
uploaded first, to `/tmp/.prometheus-inputs/<session>/<n>/...` on the
backend, and runs with the host path pointed at the copy. A file inside a
skill brings its whole skill directory (the one holding `SKILL.md`);
unchanged files are not sent again, executable bits are kept, and one
command uploads at most 512 MiB (a notice lists what was left out).

### Plugin backends

`terminal.backend = "modal"` authenticates by `terminal.modal.mode`:
`direct` (the `MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET` secrets),
`managed` (the Nous subscription sign-in, billed as
`subscription:nous`) or `auto` (default: direct when both secrets are
stored, else managed when signed in). `prometheus doctor terminal` shows
the chosen mode, or fails naming both missing options. The first-party
`singularity`, `modal`, `daytona` and `vercel` plugins themselves are not
published yet.

A `terminal-backend` plugin (world 1.1) provides `name`, `descriptor`,
`check`, `execute`, `upload`, `download`, `cancel` and `teardown`. Its name
may not be `local`, `container`, `docker`, `podman` or `ssh` (any case), and
a descriptor with a missing field or an invalid value (a relative
`default_cwd`) rejects the backend at load. An `execute` reply without an
exit code is a contract violation (an error result, never exit 0).

Background commands work on plugin backends too (`background = true`, a
trailing `&`, or a timeout above `terminal.max_foreground_secs`): the host
makes the `execute` call from a task the [`process`](../reference/tools.md#process) tool tracks. Its output
arrives when the call returns (`poll` shows nothing before that, and
`write`/`submit` have no stdin to write to); `kill`, the end of the session
and the call's timeout (plus 10 s grace) ask the plugin to `cancel` it, and
`notify_on_complete` or a promotion reports its end. A background call
without a timeout sends `timeout-ms = 18446744073709551615` (`u64::MAX`):
no limit.

Backend calls run side by side, each on an instance of its own: a running
`execute` never holds up another command, an upload or a download, and
`cancel` reaches the plugin while the `execute` it names still runs. The
plugin's `plugins.wasm.call_timeout_ms` bounds only its own computation;
time it spends waiting in host imports (HTTP to the remote sandbox) is not
counted, so an `execute` lasts as long as its command's timeout (the host
stops the call itself 30 s after that). Because instances do not share
memory, a backend keeps what must outlive one call in `state` or in its
remote service.

```console
$ prometheus config set terminal.backend acmebox
$ prometheus config validate   # exits 78
error: unknown terminal backend 'acmebox'; available: local, container, ssh
$ prometheus config set terminal.backend local
$ prometheus config validate
<...>config.toml is valid
```

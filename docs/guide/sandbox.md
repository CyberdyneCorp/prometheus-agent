# OS sandbox and terminal backends

Every local child process (terminal commands, background processes, code
execution, MCP stdio servers, hook and skill scripts) runs inside an OS
sandbox derived from the session's capability set. The sandbox is the
boundary; approvals and scanners are heuristics on top of it.

| OS | Mechanism | `fs` | `net` | `process` | `syscalls` |
|---|---|---|---|---|---|
| Linux | Landlock + seccomp | enforced (Landlock) | enforced (Landlock TCP rules, ABI 4 / Linux 6.7, plus seccomp for non-TCP IP sockets) | enforced (seccomp; signal scoping on Linux 6.12+) | enforced (seccomp) |
| macOS | Seatbelt (`sandbox-exec`) | enforced | enforced | enforced | partial (platform maximum) |
| Windows | AppContainer + Job Object | enforced (ACL entries for the container SID) | enforced (`internetClient` capability or none); partial for a child under the egress policy (below) | enforced (Job Object) | partial (platform maximum) |

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
  Windows an AppContainer reaches no loopback address (only an
  administrator can exempt one), so the proxy is out of reach and no proxy
  variables are set: a child under the egress policy gets the
  `internetClient` capability (public addresses only; the container still
  blocks loopback and the local private network) while the policy admits
  every public host (`public` mode, no `/egress deny` pattern, no host
  grant narrower than `*`, checked at each start), and no network at all
  otherwise, so an `allowlist` session's commands cannot connect even to
  allowed hosts. Host rules (metadata endpoints, private ranges beyond the
  local subnet) are not checked for those direct connections, so `net` is
  `partial`; that is the platform's maximum, not a downgrade, and needs no
  `allow_degraded` entry. A child given its own
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
its container on the granted directories. Below a directory whose ACL is not
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

Every start then logs a WARN naming the feature, and the session banner shows
the level, for example `sandbox: partial:landlock_net`.
`security.sandbox.mode = "off"` disables the sandbox and needs
`allow_degraded = ["all"]`.

## Terminal backends

`terminal.backend` selects where commands run: `local` (the sandboxed host)
in this release; `container` and `ssh` are reserved and fail with
`terminal backend 'container' is available in M2` (exit 69). Any other name
fails validation (exit 78) and lists the available backends. `prometheus
chat` (and every session start) checks the setting before the first turn,
with the same exits; Prometheus never falls back to another backend. Defaults: `timeout_secs = 180`,
`max_foreground_secs = 600`, `idle_lifetime_secs = 300`,
`persistent_shell = true`; container and SSH settings are only type-checked
while another backend is selected.

```console
$ prometheus config set terminal.backend acmebox
$ prometheus config validate   # exits 78
error: unknown terminal backend 'acmebox'; available: local, container, ssh
$ prometheus config set terminal.backend local
$ prometheus config validate
<...>config.toml is valid
```

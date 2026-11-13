# The desktop app

The desktop app (`apps/desktop`) is a thin native shell over your home's
gateway daemon. It bundles the `prometheus` binary of the same version and
renders chat and settings with the dashboard's components; sessions,
agents, approvals and configuration stay in the daemon, so closing the
app never stops a turn.

## Attaching to the daemon

On launch the app runs its bundled binary as
`prometheus daemon ensure --json`, which starts the daemon when it is not
running and prints one JSON object:

```text
{"rpc_url":"ws://127.0.0.1:53817/rpc","control":"…/run/control.sock","instance_id":"…","build_id":"…","version":"0.1.0","rpc_protocol":"1.0","features":["session-attach"]}
```

The app never owns the daemon: quitting it leaves the daemon (and its
turns) running, and the CLI sees the same daemon. Before anything has
started it:

```console
$ prometheus daemon status
gateway is not running
```

The app accepts only a plain loopback endpoint (`127.0.0.1` or `[::1]`,
an explicit port, the `/rpc` path, no credentials, query or fragment) from
a daemon that serves `session-attach` and speaks the app's protocol major;
anything else is refused as `Invalid local gateway endpoint: <reason>`.
The ensure step is bounded by 75 s; past it the app shows
`gateway daemon did not become ready within 75 s` with a link to
`<home>/logs/gateway.log` and a Retry button. Output that is not one JSON
object is reported as
`prometheus daemon ensure produced no result (exit <code>): <last stderr line>`.

When the running daemon is another build, the app restarts it (asking
first while turns are running) — unless the daemon is newer than the app:
then it keeps it and shows
`Prometheus <daemon> is running but this app bundles <bundled>; update the app`.

## Connections and tickets

Every WebSocket dial gets its own one-use ticket from the daemon's
owner-only control channel. The ticket travels only as the
`prometheus-ticket.<ticket>` subprotocol: never in a URL, the page's
storage, a log line or anything a plugin can read. At most 100 dials may
be pending at once (`Too many pending gateway dials`).

The app's client uses a 120 s request timeout, a 15 s connect timeout, a
15 s heartbeat and a 45 s dead deadline. A lost connection is redialled
after `random() * min(15000, 300 * 2^attempt)` ms; a connection that
closes within 5 s of opening counts as a failed attempt. Every reconnect
starts with `rpc.hello` as a `desktop` client and re-attaches the
window's open sessions from their last seen event. A daemon restarted by
the CLI is found again through its discovery file; a stopped daemon is
not started behind your back.

Opening a session that fails becomes a fresh draft only when the daemon
answers that the session does not exist; any other failure (for example
while reconnecting) keeps the window on the session with Retry.

## What the page may do natively

The page reaches the machine only through one typed command with an
enumerated set of requests: open an external link, open a file (guarded),
reveal a file in its folder, pick a file or directory, notify, set the
badge, register a global shortcut, read the clipboard on an explicit user
action, and check or apply an update. Plugin frames have no access to any
of them; a plugin's attempt is refused and logged at WARN with the
plugin id.

Links open only after a real primary-button click: `http:`, `https:` and
`mailto:` in your browser or mail client; `file:` links and local paths
only inside your home or the session workspace, never an executable;
everything else (`javascript:`, `data:`…) is refused. Page content can
never open a window, and the window never navigates away from the app.

Development builds have no updater: Check for updates answers
`This build doesn't get updates. Install a release build to receive updates.`

## Deep links

The app registers the `prometheus://` scheme and understands exactly:

| Link | Opens |
|---|---|
| `prometheus://session/<id>` | that session, in the profile that owns it |
| `prometheus://new?profile=<name>[&cwd=<path>]` | an empty draft |
| `prometheus://plugins/install?id=<catalog id>` | the plugin install flow |
| `prometheus://skills/install?id=<catalog id>` | the skill install flow |

A link never fills in or submits a prompt or answers a request: extra
parameters such as `prompt=` are dropped, and any other link is ignored
with a WARN log.

## Chats, commands and settings

New chats are created with `source: "desktop"`. The slash palette comes
from the daemon's command registry (`commands.catalog`): a command is
offered when it or one of its subcommands runs on the desktop, skill and
plugin commands always are, and running an unavailable one shows the
registry's reason, for example `/keys is only available in the terminal
interface.`

Each bot profile's chat comes from `bot.chat.ensure {profile}`, one
get-or-create transaction in the daemon, so two windows opening the same
bot (even across a daemon restart) land in the same chat; bot chats are
not in the sessions list.

Settings write configuration only through `config.patch {profile,
changes}`, validated as a whole and applied atomically: choosing
"Inherit from main agent" for subagents removes `delegation.model` and
the direct endpoint keys in one patch, and a schema violation leaves the
file unchanged. Every request that touches profile data names the
window's profile; switching profiles detaches the window's sessions and
dials again for the new profile. "Reload configuration" sends
`config.reload {profile}`.

## Building and testing

The shell's logic lives in `crates/pmt-desktop` (built and tested on
every target); `apps/desktop/src-tauri` binds it to Tauri 2 and is its own
Cargo workspace because the webview toolkits do not build for musl or
Android. Its page is built from `apps/desktop/src` by `web/build.mjs` into
`apps/desktop/dist` (checked in). `scripts/check` formats, lints, tests
and license-checks the app where a webview toolkit exists (Linux needs
WebKitGTK 4.1). The desktop browser tests run the page in Chromium at the
webview origin with its native command served by the real shell core.

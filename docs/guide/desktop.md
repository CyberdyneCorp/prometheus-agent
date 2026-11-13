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
action, update the tray, post an approval notification, the quick-ask
panel's requests, list plugin panels, look up and install a catalog entry
after the install dialog, and check, apply or restart for an update. Plugin frames have no access to any
of them; a plugin's attempt is refused and logged at WARN with the
plugin id.

Links open only after a real primary-button click: `http:`, `https:` and
`mailto:` in your browser or mail client; `file:` links and local paths
only inside your home or the session workspace, never an executable;
everything else (`javascript:`, `data:`…) is refused. Page content can
never open a window, and the window never navigates away from the app.

## Updates

A release build embeds the release signing key and the channel it was
built for (`stable` or `canary`). It reads the channel's feed
(`desktop-<channel>.json`, signed like every release file) at most every
6 hours, offers a newer version in a notice and under Settings → Updates,
downloads it only when you click Download and install, and verifies its
signature before anything is written: a bad one is discarded with
`Update signature invalid; not installed` and the app stays as it is. A
second apply while one runs answers `An update is already in progress.`
The verified update installs when you quit or click Restart now; the
daemon is not stopped for it (the new app finds it stale and restarts it
with the new bundled binary). Only the `.dmg` and AppImage installs are
updated this way; MSIX, Microsoft Store, deb and rpm installs are updated
by App Installer, the Store or the system package manager.

Development builds have no updater and make no request: Check for
updates answers
`This build doesn't get updates. Install a release build to receive updates.`

## Running in the background

Closing the last window keeps the app in the tray (macOS menu bar,
Windows notification area, Linux StatusNotifierItem) while
`desktop.keep_in_tray = true` (the default) and a tray exists. The tray
menu lists the pending approvals and the running sessions; quitting the
app never stops the daemon or a running turn, which you find finished on
the next launch.

An approval for a session whose window is not focused posts an OS
notification naming the session and the tool. The notification has no
buttons: clicking it brings the window forward with the approval card
focused, and the request stays pending until you answer it in the card or
its deadline settles it.

## Quick ask

`desktop.quick_ask.hotkey` (default `CommandOrControl+Shift+Space`, empty
disables it) opens a floating quick-ask panel over the current app. A
question goes to the quick-ask session of `desktop.quick_ask.profile`
(default the profile you used last), reusing that session when it was
active within `desktop.quick_ask.reuse_secs` (default 900). `Esc` hides
the panel, `Ctrl+Enter` (`Cmd+Enter`) opens the session in the main
window, and the clipboard is attached only when you press Attach
clipboard. When another application owns the chord, a notification names
it and the app runs without the hotkey.

## Launch options

The `[desktop]` section of the active profile's `config.toml` is read
before the first window opens:

| Key | Values | |
|---|---|---|
| `disable_gpu` | `auto` (default: off only on remote displays), `on`, `off` | GPU acceleration of the webview (WebKitGTK and WebView2; macOS has no switch) |
| `linux_window_backend` | `auto`, `wayland`, `x11` | `x11` opens the windows through XWayland on a Wayland session |
| `linux_secret_store` | `auto`, `secret-service`, `kwallet` | validated; the app itself keeps no secrets in a secret store |
| `accessibility_tree` | `true`, `false` | expose the webview's accessibility tree |
| `manage_launcher_entry` | `true`, `false` | Linux: keep the XDG `.desktop` entry current; `false` keeps an existing one and still creates a missing one |

An invalid value does not stop the app: it starts with that option's
default and shows, for example,
`desktop.disable_gpu: expected one of "auto", "on", "off"`.

## Settings and platform support

Settings shows which desktop features this machine cannot offer and why,
from the daemon's platform support rows and the app's own probes, and
disables their controls instead of letting them fail. For example the
quick-ask hotkey control on a GNOME Wayland session reads
`global shortcuts are not available in this Wayland session`, and the tray
option on a desktop without a StatusNotifierItem host is disabled with
that reason.

## Display preferences

Theme, light/dark mode, accent, density, status-bar items, keybindings,
saved layouts, interface mode, model-picker favourites and dismissed tips
are stored once per machine by the local daemon (`ui.prefs.get` and
`ui.prefs.patch`, file `<home>/ui-prefs.json`) and shared by every desktop
window and dashboard or webapp tab, which repaint within a moment of a
change. Credentials, drafts and per-window state (tabs, pane sizes,
scroll positions, zoom) are never stored there. A corrupt file is kept as
`ui-prefs.json.corrupt-<timestamp>` and rebuilt from the open windows. The
desktop always uses its local daemon for these, whichever connection a
window is on.

## Plugin panels

An installed, enabled plugin that declares `[ui]` (see the plugin
authoring guide) gets a panel in the window. Each panel runs in a frame on
the plugin's own origin (`plugin-<id>://localhost`, or
`http://plugin-<id>.localhost` on Windows), so its storage is its own, and
it reaches the app only through the plugin UI SDK messages for the SDK
capabilities its manifest lists: `sdk.sessions.list()` from a plugin
without `sessions.read` rejects with
`capability sessions.read not granted to plugin <id>`. A panel loads only
when its package signature was verified at install or the plugin was
installed with `--allow-unsigned`. Panels of plugins installed while the
app runs appear at the next launch.

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

An install link only opens a dialog for that catalog entry: its name,
publisher, version, signature status and the capabilities it asks for,
read from the configured catalog without downloading the entry. It is
installed into the window's profile only when you click Install. An id
that is not in the catalog, a URL, a path or a link with extra parameters
shows `Unknown catalog entry <id>` and nothing is fetched.

## Launching from the command line

```text
prometheus desktop [--cwd <path>] [--ignore-existing]
prometheus desktop --install
```

`prometheus desktop` (alias `prometheus gui`) starts the installed app for
the selected profile (`-p`) with a new chat whose workspace is `--cwd`.
Without an app it exits 69 with
`Prometheus Desktop is not installed; run 'prometheus desktop --install'`.
`--install` downloads the desktop package of this binary's version and
channel and installs it only after its signature verifies against the
release keys embedded in the binary (a bad download is dropped and the
command exits 77). `--ignore-existing` starts the app without starting a
local daemon.

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

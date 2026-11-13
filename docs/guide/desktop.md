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

## The chat window

**Transcript.** Answers stream in with a row per tool call; a tool row
whose output names an image file expands to show it, read with
`fs.read {session_id, path}` over the session's own connection (never
from a local path). Cron and delegation completions are collapsed
disclosures showing the result only. From 4 prompts on, a timeline rail
has a marker per prompt; reopening a session renders its last 10 prompts,
a marker jumps to its prompt and `Show earlier` loads the page before.
`CommandOrControl+F` searches the rendered transcript. Each session keeps
its distance from the bottom when you leave it, and new output is
followed only while the view is at the bottom.

**Drafts.** Every chat of every window keeps its composer text and staged
images across relaunches. When a chat's session was deleted elsewhere,
its draft moves into the fresh draft with a `Restored your unsent
message` strip offering Undo; an image message the daemon never
acknowledged is offered once as `Restore image draft`. Nothing is sent
for you.

**History and queue.** In an empty composer `Up`/`Down` recall the
profile's earlier prompts. Prompts sent while a turn runs wait in the
session's queue, which the daemon holds (`queue.list`, `queue.edit`,
`queue.delete`, `queue.pause`, `queue.resume`, and the `queue.changed`
event), so every window shows the same list and it survives restarts
(`<profile>/queues/<session>.json`). Stop or `Esc` interrupts the turn
and pauses the queue. A turn whose outcome a daemon restart lost shows as
`Turn lost during restart` with one action, Discard, which unblocks the
queue without sending it again.

**Status bar.** A per-session approval bypass (like `/yolo`; confirmed
when turned on, gone with the session), a context meter whose popover
lists the tokens of the system prompt, tool definitions, skills, memory,
rules, MCP, subagent definitions and the conversation
(`session.context_usage` `categories`, which sum to the meter), and
optional cache-hit and output-speed readouts chosen from the bar's
context menu. `CommandOrControl+Shift+S` hides or shows the bar.

**Model picker.** The composer lists every model of every connected
provider (`model.options {profile}` before a chat exists), favourites
first in starring order. The last pick of this device is used for new
chats without changing `model.default` (a profile with no default keeps
its first pick as the default). Switching mid-chat lasts for the session
(`/model <id> --session`) after a warning that the next message re-reads
the conversation, and asks again when the daemon flags the switch as
risky (`risky`). Each model's reasoning effort and fast preset come back
when it is picked; the pill shows `Ultra→Max` when the route clamps the
level.

**Subagents.** Settings → Model → Subagents edits the delegated agent's
provider and model, `delegation.max_iterations`,
`delegation.max_concurrent_children`,
`delegation.inactivity_timeout_secs` and `delegation.reasoning_effort` for
the profile in `Applies to` (shown with two or more profiles). Changes
stay a draft until Apply, which writes one `config.patch`; Cancel, another
page or another `Applies to` discards them, and a provider without a
model is refused unless `Use parent model` is checked.

## Workspace panes

The right sidebar holds Settings, Files, Artifacts and Preview.

**Files** lists the session's workspace through the session's own
terminal backend (`fs.list {session_id, path, show_ignored}`), so a
container session shows the container's files; `.gitignore`d entries
show only with *Show ignored files*. Text files up to 2 MiB and images
preview inline (`fs.read`); a larger file shows its first 2 MiB with
*Open externally*, which goes through the guarded file opener (local
sessions only). Expanded folders refresh after each tool call that
writes, and a tool row that names a file offers *Reveal in file tree*.

**Artifacts** lists the images, files and links successful tool calls and
answers produced (`artifacts.list {profile, query, cursor}`), each with
its session and Preview, Download, Open in browser and Copy. Files written
in the current session are suggested above the composer; a dismissed
suggestion stays dismissed (per device) until the file is written again.

**Preview** shows web pages (`http`, `https`) and workspace files in a
sandboxed frame of its own origin: it cannot reach the app's native
commands, storage or connection, and popups and downloads are refused.
*Hide* keeps the page and its state; *Close* releases it. In *Annotate*
mode a click or a drag picks an element; the agent the shell installs in
every frame redacts password and hidden values and key- or token-like
attributes in the page before describing the pick. Each saved pin keeps
its number; *Add N comments* puts each pin's snapshot (a PNG
`data:` image), selector, markup and layout styles into the composer,
grouped by top-level element beyond 5 pins, and sends nothing. Switching
chats clears the pins.

## Windows, tabs, modes and terminals

`CommandOrControl+Shift+N` opens another window and *Pop out* opens the
chat's session alone in its own window; each window attaches on its own
and streams the same output. Each window has session tabs
(`CommandOrControl+T` new, `Ctrl+Tab`/`Ctrl+Shift+Tab` cycle,
`CommandOrControl+W` close, except over a terminal, and
`CommandOrControl+Shift+T` reopen) and toggles its left and right
sidebars with `CommandOrControl+B` and `CommandOrControl+J`.

The header chooses the interface mode, `advanced` or `simple` (chat
first: the status bar, terminal, file browser, review pane, technical tool
rows and inline diffs rest out of sight, and the model's thinking starts
collapsed), and a
layout: `default`, `basic`, `focus`, `terminal-deck` and `quad` in
advanced mode, `sidebar-left` and `sidebar-right` in simple mode, or one
you saved. Each mode remembers its own arrangement.

Terminals are shells the daemon starts in the session's workspace
(`pty.open`, `pty.attach`, `pty.input`, `pty.resize`, `pty.close`,
`pty.list`; output arrives as `pty.output`). Only drivers type; a daemon
keeps at most 16, a shell outlives its last attached window by 15 minutes,
reattaching replays up to 1 MiB of output, and closing the tab ends the
shell. *Add to chat* puts the selected output in the composer.

While the session has running subagents, a frame above the composer shows
them; a selected child can be steered (`subagent.steer`, queued for its
next checkpoint) or stopped (`subagent.stop`).

## Review, memory graph and HUD

`CommandOrControl+G` toggles the review pane of the chat's repository:
its branch, how far it is ahead of and behind upstream, and the changed
files with their diffs in one of three scopes. `uncommitted` compares
`HEAD` with the working tree and is the only scope that can stage,
unstage, revert (after an in-window confirmation) and commit;
`branch` shows the branch's commits since its base and `last-turn`
everything that changed since the most recent turn in the repository
began, commits made during it included. Each turn start snapshots the
working tree as a tree object (nothing else in the repository changes).
*Commit & Push*, branch create/switch and *Create PR* (offered only when
`gh` is installed) run through the session's terminal backend; *Generate
commit message* asks the `commit_message` auxiliary route. The pane needs
the workspace on this machine (a container or SSH session's repository is
not reviewed). Hooks do not run for the pane's own git commands.

`/journey` (also `/learning`, `/memory-graph`, the palette or
`CommandOrControl+Shift+M`) opens the memory graph: learned skills and
memories over time, with a playback scrubber and the `all`, `used` and
`learned` filters. Editing a memory or deleting a node goes through
`manage.change` like every surface (a deleted skill is archived); open
chats keep their snapshot. *Share* gives a code of node ids and positions
only.

`CommandOrControl+Shift+H` detaches the chat into the HUD: a chrome-free,
always-on-top bar with the live transcript and a composer. Press and hold
the handle to move it, drag its edges to resize it, *Reset size* puts it
back. `desktop.hud.snap_hotkey` (default `CommandOrControl+Shift+G`)
snaps it to the pointer and `desktop.hud.tap_to_summon = true` lets a tap
(a left click) on the tray icon summon it. A prompt sent from the HUD
carries the application, title and bounds of the window underneath it,
never pixels (macOS needs the Screen Recording permission for titles). On
native Wayland snapping and the window underneath are unavailable, and
HUD settings say why.

## Management panes, projects and shortcuts

The *Manage* pane has Skills (enable toggles and the official optional
catalog), Scheduled jobs (with an `All profiles` view), Profiles (switch,
export, import), Messaging, Agents (each profile's bot chat) and the
Command Center (running sessions, pending approvals and active subagents
of every profile). Every request names its profile: the `Applies to`
choice (shown with two or more profiles; it follows the window's
profile), or a job's owning profile in the all-profiles view. A profile
other than the window's is reached over a connection of its own (a
ticket grants one profile).

Messaging shows each adapter in one state: `not_configured`, `saved`
(credentials stored, disabled), `pending_restart`, `connecting`,
`connected`, `error` with the reason, or `daemon_stopped`. After a
credential save, clear or enable toggle a *Restart adapter* banner stays
until the adapter restarted (with *Retry* when that failed). Telegram's
*Create with QR* runs the daemon's onboarding flow: scan the code (or open
the link), and the token is stored, your Telegram id becomes the only
allowed user and only the Telegram adapter restarts; closing the dialog
cancels it and saves nothing.

The Projects sidebar lists explicit projects, repositories of the
profile's sessions and repositories the background scan found
(`projects.repo_scan_*`; on by default in the desktop), each with one
click to create a project. *Discover repositories* sets
`projects.repo_scan_enabled`; off, discovered rows disappear at once.

`CommandOrControl+K` or `CommandOrControl+P` opens the command palette
(pages, settings, sessions by title or id, actions such as *Export
profile*). Manage → Shortcuts rebinds every shortcut, plugin panels'
included (`register {shortcuts: [{id, label, chord}]}` in the panel SDK);
two actions on one chord are both flagged and neither runs until one
changes. Bindings are a machine-wide display preference; the zoom
(`CommandOrControl+=`, `-`, `0`, half a step each) belongs to the window.
`desktop.font_family` goes in front of the theme's fonts and
`desktop.terminal_font_family` sets the terminal's (empty: the default
monospace fonts). Exporting a profile runs `profile export` without
secrets and adds the appearance (theme, mode, shortcuts, zoom) as an
overlay that an import applies again.

## Building and testing

The shell's logic lives in `crates/pmt-desktop` (built and tested on
every target); `apps/desktop/src-tauri` binds it to Tauri 2 and is its own
Cargo workspace because the webview toolkits do not build for musl or
Android. Its page is built from `apps/desktop/src` by `web/build.mjs` into
`apps/desktop/dist` (checked in). `scripts/check` formats, lints, tests
and license-checks the app where a webview toolkit exists (Linux needs
WebKitGTK 4.1). The desktop browser tests run the page in Chromium at the
webview origin with its native command served by the real shell core.

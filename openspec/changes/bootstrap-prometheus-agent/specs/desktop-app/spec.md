# Desktop App

## Purpose
The desktop app is a thin native shell (system webview) over the profile's gateway daemon: it bundles the `prometheus` binary of the same version, attaches to the daemon through the client protocol owned by the `workspace-architecture` capability (with session-attachment semantics from `tui`), and renders chat, settings and plugins with the same UI components as the web dashboard. The key decision is that the shell owns only machine integration (windows, tray, hotkeys, notifications, OS dialogs, updates) behind an explicit allowlist of native commands, while sessions, agents, approvals and configuration live in the daemon.

## ADDED Requirements

### Requirement: Daemon attachment through the bundled binary
On launch the desktop SHALL run its bundled `prometheus daemon ensure --json` (with `--profile <name>` when a profile is selected), parse exactly one JSON object from stdout, and attach to the endpoint it reports; it SHALL NOT start its own agent process or own the daemon's PID, and output that is not one JSON object SHALL fail with `prometheus daemon ensure produced no result (exit <code>): <last stderr line>`.
**Milestone:** M3

#### Scenario: Ensure prints garbage
- **WHEN** `daemon ensure` exits 2 with a non-JSON stdout
- **THEN** the desktop SHALL show the no-result error with the last stderr line and a Retry button, and SHALL NOT attach

### Requirement: Ensure is bounded
The ensure step SHALL be bounded by 60 s plus a 15 s grace, and an expired ensure SHALL be reported as `gateway daemon did not become ready within 75 s` and SHALL never be treated as success.
**Milestone:** M3

#### Scenario: Cold start exceeds the deadline
- **WHEN** the daemon is not ready after 75 s
- **THEN** the desktop SHALL show the timeout diagnostic with a link to `<home>/logs/daemon.log` and SHALL NOT attach

### Requirement: Local endpoint validation
For the app-managed `local` connection the desktop SHALL accept an ensure endpoint only when its origin is `http` on `127.0.0.1` or `[::1]` with an explicit port and no credentials, path, query or fragment, and it carries a non-empty `instance_id`, an `rpc_protocol` whose major the desktop speaks, and the `session-attach` feature; otherwise it SHALL refuse with `Invalid local gateway endpoint: <reason>`; non-loopback backends SHALL be reachable only as registered `remote` or `ssh` connections under the requirement Remote connections are authenticated and encrypted, never by an ensure result.
**Milestone:** M3

#### Scenario: Non-loopback endpoint
- **WHEN** ensure reports `http://192.168.1.5:9120`
- **THEN** the desktop SHALL refuse with `Invalid local gateway endpoint: not loopback`

### Requirement: Version skew restarts a stale daemon
When the attached daemon's `build_id` differs from the bundled binary's, the desktop SHALL ask the user before restarting it when any session has a running turn, restart it without asking otherwise, and re-attach; when the bundled binary is older than the daemon it SHALL NOT downgrade the daemon and SHALL instead show `Prometheus <daemon> is running but this app bundles <bundled>; update the app`.
**Milestone:** M3

#### Scenario: Daemon outlived a CLI update to an older app
- **GIVEN** the daemon runs 1.6.0 and the app bundles 1.5.2
- **WHEN** the desktop attaches
- **THEN** it SHALL keep the daemon running and show the update-the-app notice

### Requirement: One-use dial tickets never reach the webview's storage
The shell SHALL obtain a one-use ticket per WebSocket dial from the daemon's owner-only control socket, bound to the purpose (`interactive` or `http`), the requesting window and the target path, expiring after the 30 s defined by `workspace-architecture`, with at most 100 pending tickets; the ticket SHALL travel only as the WebSocket subprotocol `prometheus-ticket.<ticket>` alongside `prometheus-rpc.v<major>` and SHALL never appear in URLs, logs, persisted state or anything readable by plugins.
**Milestone:** M3

#### Scenario: Replayed ticket
- **WHEN** a second socket presents an already consumed ticket
- **THEN** the daemon SHALL close it with 4401

#### Scenario: Ticket flood
- **GIVEN** 100 unconsumed tickets pending
- **WHEN** another dial is prepared
- **THEN** the shell SHALL fail it with `Too many pending gateway dials`

### Requirement: Shared client defaults
The desktop's protocol client SHALL use a 120 s request timeout, a 15 s connect timeout, the 15 s heartbeat and 45 s dead deadline of the client protocol, and SHALL send `rpc.hello` with `client: "desktop"` and `capabilities.server_requests = true` on every connection, including reconnects.
**Milestone:** M3

#### Scenario: Reconnect re-advertises
- **WHEN** the socket reconnects
- **THEN** the first frame SHALL be `rpc.hello` with `capabilities.server_requests = true`

### Requirement: Reconnect backoff with full jitter
The desktop SHALL reconnect with delay `random() * min(15000, 300 * 2^attempt)` ms, SHALL count a connection that closes within 5 s of opening as a failed attempt, and SHALL re-attach every session that was open in a window with its `last_seen_seq`.
**Milestone:** M3

#### Scenario: Flapping daemon
- **WHEN** the socket opens and closes after 2 s three times
- **THEN** the attempt counter SHALL be 3 and the next delay ceiling SHALL be 2400 ms

### Requirement: Native command allowlist
The webview SHALL reach native functionality only through an enumerated set of shell commands (open external URL, open file with guard, reveal in folder, pick file, pick directory, notify, set badge, register hotkey, read clipboard on explicit user action, check and apply update), each with typed arguments; any other invocation SHALL be rejected, and plugin frames SHALL have no access to native commands at all.
**Milestone:** M3

#### Scenario: Plugin tries a native command
- **WHEN** code inside a plugin frame invokes `reveal_in_folder`
- **THEN** the call SHALL be rejected and logged at WARN with the plugin id

### Requirement: Windows never open themselves
Every window SHALL deny `window.open` and new-window navigation from page content; top-level navigation away from the app origin SHALL be blocked; links SHALL open externally only after a trusted primary-button click and only through the external-URL command.
**Milestone:** M3

#### Scenario: Artifact script opens a window
- **WHEN** rendered agent content calls `window.open('https://attacker.example')`
- **THEN** nothing SHALL open and no external browser SHALL launch

### Requirement: External URL scheme allowlist
The external-URL command SHALL open only `http:`, `https:` and `mailto:` URLs in the OS handler, SHALL route `file:` URLs and bare local paths to the guarded file opener (which refuses executables and paths outside the user's home and the session workspace), and SHALL reject every other scheme with `{ok: false, reason: "invalid"}`.
**Milestone:** M3

#### Scenario: javascript link from the agent
- **WHEN** the user clicks a `javascript:alert(1)` link in a reply
- **THEN** the command SHALL return `{ok: false, reason: "invalid"}` and nothing SHALL execute

### Requirement: Sessions are created with desktop source
The desktop SHALL create sessions with `source: "desktop"` and SHALL key desktop-only agent capabilities (preview, window read) off the session's source and the attached client's advertised features, never off environment variables.
**Milestone:** M3

#### Scenario: New chat
- **WHEN** the user starts a new chat
- **THEN** `session.create` SHALL carry `source: "desktop"`

### Requirement: Gone-session verdict
A failed resume SHALL fall back to a fresh draft only when the daemon answers `-32003` (not found) for the session id and no profile switch or reconnect is in flight; any other failure SHALL keep the window on that session with a Retry action.
**Milestone:** M3

#### Scenario: Resume during reconnect
- **GIVEN** the socket is reconnecting
- **WHEN** a resume fails
- **THEN** the window SHALL stay on the session and offer Retry, not open an empty draft

### Requirement: Slash palette from one command registry
The slash palette SHALL be populated from `commands.catalog`, where each command's per-surface disposition (`exec`, `unavailable` with a reason, `hidden`) and per-subcommand disposition are authored once in the command registry; the palette SHALL suggest a command when it or any of its subcommands is executable on desktop, SHALL always suggest extension commands (skills, plugin commands), and SHALL render an unavailable command's registry-provided reason verbatim when it is executed.
**Milestone:** M3

#### Scenario: Subcommand available, parent not
- **GIVEN** `/skills` is `unavailable` on desktop except subcommand `pending`
- **WHEN** the user types `/skills p`
- **THEN** the palette SHALL suggest `/skills pending`

#### Scenario: Terminal-only command
- **WHEN** the user runs `/keys`
- **THEN** the desktop SHALL show `/keys is only available in the terminal interface.`

### Requirement: Bot chats are created atomically by the daemon
Each bot profile's persistent chat SHALL be obtained with `bot.chat.ensure {profile}`, which the daemon SHALL implement as a single get-or-create transaction keyed by `(profile, "bot-chat")` so concurrent callers and callers racing a daemon restart receive the same session; the desktop SHALL never create a bot chat with `session.create`, and the bot chat SHALL be hidden from the sessions list unless `include_hidden` is requested.
**Milestone:** M3

#### Scenario: Two windows open the same bot during a restart
- **WHEN** two windows call `bot.chat.ensure` for profile `helper` while the daemon is starting
- **THEN** both SHALL receive the same `session_id` and exactly one bot chat SHALL exist

### Requirement: Settings write validated config patches
Settings screens SHALL write configuration only through `config.patch {profile, changes}`, which validates the whole patch against the config schema and applies it atomically; coupled fields (for example `delegation.model` and `delegation.provider`) SHALL be written in one patch, and a schema violation SHALL leave the file unchanged and show the validator's message next to the field.
**Milestone:** M3

#### Scenario: Clearing the subagent route
- **WHEN** the user selects "Inherit from main agent"
- **THEN** one `config.patch` SHALL remove both `delegation.model` and `delegation.provider`

### Requirement: Profile switching is explicit
Every desktop request that touches profile data SHALL name the window's profile; switching profiles SHALL detach the window's sessions, request tickets for the new profile and re-attach, and a window SHALL never show data from two profiles at once.
**Milestone:** M3

#### Scenario: Restart gateway under a profile
- **GIVEN** the window's profile is `coder`
- **WHEN** the user triggers a config reload
- **THEN** the request SHALL carry `profile: "coder"`

### Requirement: Closing windows does not stop the agent
Closing the last window SHALL keep the app in the tray (macOS menu bar, Windows notification area, Linux StatusNotifierItem where available) when `desktop.keep_in_tray = true` (default true), and quitting the app SHALL never stop the daemon or interrupt running turns; the tray menu SHALL show pending approvals and running sessions.
**Milestone:** M3

#### Scenario: Quit during a turn
- **GIVEN** a running turn
- **WHEN** the user quits the desktop app
- **THEN** the turn SHALL continue in the daemon and SHALL be visible on next launch

### Requirement: Approval notifications never approve
When a server request arrives for a session whose window is not focused, the desktop SHALL post an OS notification naming the session and the tool; clicking it SHALL focus the approval card, and no notification action SHALL approve or deny on the user's behalf.
**Milestone:** M3

#### Scenario: Background approval
- **GIVEN** the app window is minimised
- **WHEN** the agent requests approval for `terminal`
- **THEN** a notification SHALL appear and the approval SHALL stay pending until answered in a card or settled by its deadline

### Requirement: Plugin UI contributions are isolated
Desktop UI contributions SHALL come only from installed, enabled plugins that declare a `ui` entry in their manifest; each SHALL load in a sandboxed frame on its own origin `plugin-<id>.localhost`, SHALL talk to the app only through the plugin UI SDK message channel (which exposes the capabilities granted to that plugin and nothing else), and SHALL get storage isolated by that origin; a contribution from a plugin whose package signature was not verified SHALL load only when that plugin was installed with `--allow-unsigned` (as recorded on its install record under the `plugin-system` signature policy).
**Milestone:** M3

#### Scenario: Plugin reads another plugin's storage
- **WHEN** plugin `a` reads `localStorage` in its frame
- **THEN** it SHALL see only keys written by plugin `a`

#### Scenario: Ungranted capability
- **GIVEN** plugin `a` was not granted `sessions.read`
- **WHEN** it calls `sdk.sessions.list()`
- **THEN** the call SHALL reject with `capability sessions.read not granted to plugin a`

### Requirement: Global hotkey quick-ask (upgrade)
The desktop SHALL register the global shortcut `desktop.quick_ask.hotkey` (default `CommandOrControl+Shift+Space`; empty disables) that opens a floating quick-ask panel over the current app; submitting SHALL send the text to the quick-ask session of `desktop.quick_ask.profile` (default the last used profile), reusing the previous quick-ask session when it was active within `desktop.quick_ask.reuse_secs` (default 900) and creating a new one otherwise; `Esc` SHALL hide the panel, `CommandOrControl+Enter` SHALL open the session in the main window, and the current selection or clipboard SHALL be attached only when the user presses the attach control.
**Milestone:** M3

#### Scenario: Hotkey already taken
- **WHEN** registration fails because another app owns the shortcut
- **THEN** the desktop SHALL show a notification naming the conflicting chord and keep running without the hotkey

#### Scenario: Quick follow-up
- **GIVEN** a quick-ask 5 minutes ago
- **WHEN** the user asks again through the hotkey
- **THEN** the question SHALL be submitted to the same quick-ask session

### Requirement: Honest platform support in settings
Settings SHALL show, from the daemon's `platform_support` and the shell's own probes, which desktop features are unavailable on the current OS and why (for example global shortcuts on Wayland compositors without the global-shortcuts portal, or tray icons without StatusNotifierItem), and the corresponding controls SHALL be disabled rather than failing at use.
**Milestone:** M3

#### Scenario: Wayland without the portal
- **GIVEN** a GNOME Wayland session without the global-shortcuts portal
- **WHEN** the user opens Settings
- **THEN** the quick-ask hotkey control SHALL be disabled with the reason `global shortcuts are not available in this Wayland session`

### Requirement: Deep links
The desktop SHALL register the `prometheus://` URL scheme and handle exactly `prometheus://session/<id>` (open that session in the profile that owns it), `prometheus://new?profile=<name>[&cwd=<path>]` (open a draft), `prometheus://plugins/install?id=<catalog id>` and `prometheus://skills/install?id=<catalog id>` (open the install flow described in the requirement Install deep links open a consent flow); any other path SHALL be ignored with a WARN log, and a deep link SHALL never submit input or answer a request.
**Milestone:** M3

#### Scenario: Link with a prompt payload
- **WHEN** the OS opens `prometheus://new?profile=work&prompt=rm%20-rf`
- **THEN** the desktop SHALL open an empty draft for `work` and SHALL NOT pre-fill or submit the prompt

### Requirement: Signed in-app updates
Release builds SHALL check the configured channel (`stable` or `canary`, the `distribution-and-update` channel names) at most once every 6 hours, SHALL download only after the user accepts, SHALL verify the update signature against the embedded public key before installing, SHALL refuse a concurrent apply with `An update is already in progress.`, and SHALL install on next quit or on explicit Restart without stopping the daemon until the new bundled binary is in place.
**Milestone:** M3

#### Scenario: Bad signature
- **WHEN** a downloaded update's signature does not verify
- **THEN** the desktop SHALL discard it, keep the current version and show `Update signature invalid; not installed`

### Requirement: Development builds have no updater
A build without an embedded release signing key (local or commit builds) SHALL report the update mechanism as `none`, and check or apply SHALL return `This build doesn't get updates. Install a release build to receive updates.`
**Milestone:** M3

#### Scenario: Apply on a commit build
- **WHEN** the user clicks Check for updates on a commit build
- **THEN** the desktop SHALL show the no-updates message and make no network request

### Requirement: Install deep links open a consent flow
A `prometheus://plugins/install?id=<catalog id>` or `prometheus://skills/install?id=<catalog id>` link SHALL only open an install dialog for that signed catalog entry showing its name, publisher, version, signature status and the capabilities it requests, and SHALL install only after the user confirms in that dialog; ids that are not in the configured catalog, URLs, paths or extra query parameters SHALL be refused with `Unknown catalog entry <id>` and nothing SHALL be downloaded before confirmation.
**Milestone:** M3

#### Scenario: Website install button
- **WHEN** the OS opens `prometheus://plugins/install?id=kanban-extras`
- **THEN** the desktop SHALL show the install dialog with the plugin's requested capabilities and SHALL install it only after the user clicks Install

#### Scenario: Link pointing at an arbitrary URL
- **WHEN** the OS opens `prometheus://plugins/install?id=https://evil.example/p.wasm`
- **THEN** the desktop SHALL show `Unknown catalog entry https://evil.example/p.wasm` and SHALL make no network request for it

### Requirement: Launching the desktop from the CLI
`prometheus desktop` (alias `prometheus gui`) SHALL launch the installed desktop app for the selected profile, passing `--cwd <path>` as the initial workspace of new chats through the `prometheus://new` deep link, and SHALL exit 69 with `Prometheus Desktop is not installed; run 'prometheus desktop --install'` when no app bundle is found; `--install` SHALL download the bundle matching the binary's version and channel, verify its signature against the embedded key before installing, and `--ignore-existing` SHALL start the app without ensuring a local daemon so the user can pick a remote connection.
**Milestone:** M3

#### Scenario: Launch in a project directory
- **WHEN** the user runs `prometheus desktop --cwd ~/src/api`
- **THEN** the app SHALL open a draft whose workspace is `~/src/api`

#### Scenario: Tampered download
- **WHEN** the downloaded bundle's signature does not verify
- **THEN** the command SHALL delete it, install nothing and exit 77

### Requirement: Desktop launch options
The shell SHALL read launch options from the `desktop` section of the active profile's `config.toml` before its first window opens: `desktop.disable_gpu` (`"auto"` default, disables GPU acceleration only on remote displays; `"on"`; `"off"`), `desktop.linux_window_backend` (`"auto"` default, `"wayland"`, `"x11"`), `desktop.linux_secret_store` (`"auto"` default, `"secret-service"`, `"kwallet"`), `desktop.accessibility_tree` (default true) and `desktop.manage_launcher_entry` (default true, Linux: keep the XDG `.desktop` entry current; false preserves an existing entry and still creates a missing one); an invalid value SHALL show a notice naming the key and allowed values and launch with that option's default.
**Milestone:** M3

#### Scenario: Compositor ignores always-on-top
- **GIVEN** `desktop.linux_window_backend = "x11"`
- **WHEN** the app starts on a Wayland session
- **THEN** it SHALL open its windows through XWayland

#### Scenario: Typo in a launch option
- **WHEN** `desktop.disable_gpu = "yes"`
- **THEN** the app SHALL launch with `auto` and show `desktop.disable_gpu: expected one of "auto", "on", "off"`

### Requirement: Machine-wide UI preferences
Display preferences (theme, light/dark mode, accent, density, status-bar items, keybindings, saved layouts, interface mode, model-picker favourites, dismissed tips and tours) SHALL be stored once per machine by the local daemon through `ui.prefs.get` and `ui.prefs.patch` and shared by every desktop window and webapp tab on that machine, propagating to open windows within 2 s; credentials, grants, drafts and per-window state (open tabs, pane sizes, scroll positions, zoom) SHALL never be written there, a corrupt file SHALL be kept as `.corrupt-<timestamp>` and rebuilt from the open windows' values, and a client meeting the store for the first time SHALL only add keys the store lacks, never overwrite existing ones.
**Milestone:** M3

#### Scenario: Theme changed in the webapp
- **GIVEN** a desktop window and a webapp tab on the same machine
- **WHEN** the user switches to dark mode in the webapp tab
- **THEN** the desktop window SHALL repaint in dark mode within 2 s

#### Scenario: Remote connection keeps local preferences
- **GIVEN** the desktop is switched to a remote connection
- **WHEN** the user changes the accent colour
- **THEN** the change SHALL be stored on this machine, not on the remote daemon

### Requirement: Chat transcript rendering
The chat view SHALL stream assistant text and structured tool-activity rows, let the user expand a tool row to view images the tool produced (fetched through the session's own connection, never from a local path for a remote session), render cron and delegation completions as collapsed disclosures whose body is the result only, show a timeline rail with one marker per user prompt once a session has 4 or more turns (clicking jumps to that prompt, `Show earlier` pages backwards), open a find bar on `CommandOrControl+F` that searches the rendered transcript, and restore each session's saved distance from the bottom on return, following new output only when the session was left at the bottom.
**Milestone:** M3

#### Scenario: Jump from the timeline
- **GIVEN** a session with 30 turns
- **WHEN** the user clicks the marker of prompt 5 on the timeline rail
- **THEN** the transcript SHALL scroll to prompt 5 and `Show earlier` SHALL load the turns before it on demand

#### Scenario: Remote image
- **GIVEN** a session on a remote connection whose tool produced `/srv/out/chart.png`
- **WHEN** the user expands the tool row
- **THEN** the image SHALL be downloaded through that connection and no local file SHALL be opened

### Requirement: Drafts are durable and never sent for the user
Each chat SHALL keep its own composer draft (text and staged attachments) per window, hidden tabs SHALL update their drafts without moving the visible composer's caret, and a draft whose session no longer exists SHALL be carried into the fallback draft with a one-time `Restored your unsent message` strip offering Undo; an image message not acknowledged before the app closed SHALL be offered once as `Restore image draft` in its original chat; none of these SHALL submit, navigate or focus on the user's behalf.
**Milestone:** M3

#### Scenario: Session deleted elsewhere
- **GIVEN** a draft typed into a session that was deleted from the CLI
- **WHEN** the desktop falls back to a fresh draft
- **THEN** the typed text SHALL appear in the new composer with the restore strip and nothing SHALL be sent

### Requirement: Composer history and queue editing
In an empty composer `Up`/`Down` SHALL recall the profile's previous prompts, and queued prompts SHALL be shown above the composer from the daemon-held queue of the session (`queue.list`, `queue.edit`, `queue.delete`, `queue.pause`, `queue.resume`), so every attached window sees the same list and it survives app and daemon restarts; pressing Stop or `Esc` while prompts are queued SHALL interrupt the running turn and pause the queue, and a turn whose outcome was lost in a daemon restart SHALL be shown as `Turn lost during restart` with a single Discard action that unblocks the queue without re-sending it.
**Milestone:** M3

#### Scenario: Edit a queued prompt
- **GIVEN** two prompts queued behind a running turn
- **WHEN** the user edits the second one in one window
- **THEN** a second window attached to the same session SHALL show the edited text

#### Scenario: Lost turn blocks the queue
- **GIVEN** the daemon restarted during a turn with one prompt queued behind it
- **WHEN** the desktop re-attaches
- **THEN** the lost turn SHALL show Discard and the queued prompt SHALL not start until the user discards it

### Requirement: Desktop status bar
The chat status bar SHALL offer a per-session approval-bypass toggle equivalent to `/yolo` (asking for confirmation when turning it on and never persisting it past the session), a context meter showing the percentage of the context window in use whose popover breaks tokens down by category (system prompt, tool definitions, skills, memory, rules, MCP, subagent definitions, conversation) from `session.context_usage`, and optional readouts for cache-hit rate (share of prompt tokens served from the provider cache this session) and output tokens per second (averaged over the last 10 model calls), both off by default; items SHALL be chosen from the bar's context menu and `CommandOrControl+Shift+S` SHALL hide or show the bar.
**Milestone:** M3

#### Scenario: Inspecting context use
- **WHEN** the user clicks the context meter
- **THEN** the popover SHALL list each category's tokens and their sum SHALL equal the meter's token count

#### Scenario: Bypass is session-scoped
- **GIVEN** the user turned approval bypass on for session A
- **WHEN** the user opens a new session B
- **THEN** session B SHALL use the profile's configured approval mode

### Requirement: Composer model picker
The composer model picker SHALL list every provider and model the daemon's catalog reports, remember the last pick per device and apply it to new chats without writing the profile default (except on a profile with no `model.default`, where the first pick SHALL be persisted), scope a mid-chat switch to the current session after warning that the next message re-reads the whole conversation at full input price, ask for confirmation when the daemon flags the switch as risky, keep starred favourites in a Favourites section in starring order, re-apply each model's remembered reasoning effort and fast-mode preset when it is picked, and show a reasoning pill that names both the requested and the sent level when the route clamps it (for example `Ultra→Max`).
**Milestone:** M3

#### Scenario: Picker does not change the default
- **GIVEN** a profile with `model.default = "model-a"`
- **WHEN** the user picks `model-b` in the composer and starts a new chat
- **THEN** the new chat SHALL use `model-b` and `config.toml` SHALL still say `model-a`

#### Scenario: Favourite for a disconnected provider
- **GIVEN** a starred model whose provider is signed out
- **WHEN** the provider is signed in again
- **THEN** the model SHALL reappear in Favourites at its starred position

### Requirement: Subagent route settings are drafts until applied
Settings → Model → Subagents SHALL edit the delegated agent's provider, model, turn limit, concurrency, timeout and reasoning effort for the profile chosen in `Applies to`, SHALL keep provider and model changes as a local draft until Apply (Cancel, leaving the page or changing `Applies to` discards it), SHALL refuse a provider change without a model or an explicit `Use parent model`, and SHALL write the route as one `config.patch`.
**Milestone:** M3

#### Scenario: Leaving without applying
- **GIVEN** an unapplied provider change
- **WHEN** the user navigates to another settings page
- **THEN** the draft SHALL be discarded and `config.toml` SHALL be unchanged

### Requirement: File browser pane
The file browser SHALL show the session workspace as a lazily loaded tree listed through the session's own terminal backend (`fs.list {session_id, path}`), hide entries ignored by `.gitignore` unless the user toggles them on, preview text files up to 2 MiB and common image formats inline, open other files only through the guarded file opener, refresh changed entries after each tool call that writes in the workspace, and offer Reveal in file tree from file references in the transcript.
**Milestone:** M3

#### Scenario: Container session
- **GIVEN** a session whose terminal backend is a container
- **WHEN** the user expands `src/`
- **THEN** the entries SHALL come from the container's filesystem

#### Scenario: Large file preview
- **WHEN** the user selects a 40 MiB log file
- **THEN** the pane SHALL show the first 2 MiB with a notice and an Open externally action

### Requirement: Artifacts gallery and suggestions
The desktop SHALL index the images, files and links produced by successful tool calls and replies into a searchable Artifacts view (`artifacts.list {profile, query, cursor}`) where each entry names its session with a jump back to it and offers preview, download, open-in-browser and copy; it SHALL show preview suggestions above the composer for files written in the current session, never for read-only inspection or failed writes, and dismissing a suggestion SHALL persist across reloads without deleting the file.
**Milestone:** M3

#### Scenario: Dismissed suggestion stays dismissed
- **GIVEN** the user dismissed the suggestion for `report.html`
- **WHEN** the app reloads
- **THEN** the suggestion SHALL not reappear until a later tool call writes `report.html` again

### Requirement: Preview rail and in-app browser
The preview rail SHALL render web pages, files and tool outputs beside the chat in a separate isolated webview that has no access to native commands, the app's storage or the client protocol, SHALL deny popups and downloads without a user gesture, SHALL load only `http`, `https` and workspace `file` URLs, and SHALL offer Hide (body stays mounted with its state, inert to shortcuts) distinct from Close (releases the page); remote sessions' files SHALL be fetched through the session's connection.
**Milestone:** M3

#### Scenario: Hidden page keeps its form state
- **GIVEN** a half-filled form in the in-app browser
- **WHEN** the user hides and restores the pane
- **THEN** the form input SHALL be intact and the page SHALL not reload

#### Scenario: Page calls a native command
- **WHEN** a page in the preview rail tries to invoke `reveal_in_folder`
- **THEN** the call SHALL fail because the preview webview has no native command channel

### Requirement: Annotate mode in the in-app browser
The in-app browser SHALL offer an Annotate mode in which the user clicks an element or drags a box and types a note, each saved note staying as a numbered pin that keeps its number when another pin is deleted; saving a pin SHALL never submit a turn, and `Add N comments` SHALL attach to the composer, for each pin, a cropped screenshot, the element's CSS selector, its markup and its layout-relevant computed styles, with password and hidden field values and attributes that look like keys or tokens redacted in the page before leaving it, grouped by top-level DOM subtree when there are more than 5 pins; switching chats SHALL clear the pins.
**Milestone:** M3

#### Scenario: Password field redaction
- **GIVEN** a pin on a login form containing a filled password field
- **WHEN** the user clicks `Add 1 comment`
- **THEN** the attached markup SHALL contain the field with its value redacted

#### Scenario: Pins do not send
- **WHEN** the user saves three pins
- **THEN** no turn SHALL start until the user presses Send

### Requirement: Windows, tabs and panes
The desktop SHALL support several windows (`CommandOrControl+Shift+N`), pop-out windows showing a single session without the sidebar, session tabs (`CommandOrControl+T` new, `Ctrl+Tab`/`Ctrl+Shift+Tab` cycle, `CommandOrControl+W` close except over an interactive terminal, `CommandOrControl+Shift+T` reopen last closed) and toggleable left and right sidebars (`CommandOrControl+B`, `CommandOrControl+J`); every window showing a session SHALL stream its live output, and each window SHALL own one attachment per open session.
**Milestone:** M3

#### Scenario: Popped-out session
- **GIVEN** a running turn in session A
- **WHEN** the user pops A out into its own window
- **THEN** both windows SHALL stream the turn with identical `seq`

### Requirement: Interface modes and layouts
The desktop SHALL offer interface modes `advanced` (default) and `simple` (chat-first: status bar, terminal, file browser, review pane, technical tool view and inline diffs rest out of sight; thinking starts collapsed) and layout presets `default`, `basic`, `focus`, `terminal-deck` and `quad` in advanced mode and `sidebar-left`, `sidebar-right` in simple mode, plus user-saved layouts; applying a layout SHALL open every pane it places and close the ones it leaves resting, each mode SHALL remember its own arrangement, and every keyboard shortcut SHALL keep working in simple mode.
**Milestone:** M3

#### Scenario: Returning to advanced mode
- **GIVEN** a custom advanced arrangement and a switch to simple mode
- **WHEN** the user switches back to advanced
- **THEN** the previous advanced arrangement SHALL be restored, not the preset

### Requirement: Embedded terminals are daemon-owned PTYs
The terminal pane SHALL host interactive shells that the daemon serving the session's profile spawns as the user in the session workspace (`pty.open`, `pty.attach`, `pty.input`, `pty.resize`, `pty.close`), so they work on local, remote and webapp connections alike; only driver attachments SHALL open or write to a PTY, hiding the pane SHALL keep shells running, a detached shell SHALL be kept for 15 minutes, at most 16 PTYs SHALL exist per daemon, reattaching SHALL replay up to 1 MiB of recent output, and closing the tab SHALL end the shell; selected output SHALL be attachable to the composer with Add to chat.
**Milestone:** M3

#### Scenario: Seventeenth terminal
- **GIVEN** 16 open PTYs
- **WHEN** the user opens another terminal
- **THEN** the daemon SHALL refuse with `terminal limit reached (16); close a terminal first`

#### Scenario: Observer cannot type
- **GIVEN** a webapp tab attached as observer
- **WHEN** it sends `pty.input`
- **THEN** the daemon SHALL answer `-32002 observers cannot write to terminals`

### Requirement: Live subagents frame
While a session has running subagents the desktop SHALL show a frame above its composer with their count and, for up to 3 children, task name, elapsed time and latest activity, expandable to the full roster; selecting a child SHALL show its details with Steer (`subagent.steer {session_id, subagent_id, text}`, acknowledged as queued for the child's next checkpoint) and Stop (`subagent.stop`) controls, and each frame SHALL belong to its own chat including in split panes.
**Milestone:** M3

#### Scenario: Steering is acknowledged as queued
- **WHEN** the user steers a running child
- **THEN** the frame SHALL show `guidance queued for next checkpoint`, not that the child has read it

### Requirement: Git review pane
For a session whose workspace is inside a Git repository, `CommandOrControl+G` SHALL toggle a review pane showing branch, ahead/behind counts and changed files with diffs scoped to `uncommitted`, `branch` or `last-turn` (everything changed since the most recent turn in this repository began, including commits made during it); stage, unstage, revert (after confirmation) and commit SHALL apply only to uncommitted changes, `branch` and `last-turn` SHALL be read-only, Commit & Push and branch create/switch SHALL run through the session's terminal backend, Generate commit message SHALL use the auxiliary model, and Create PR SHALL be offered only when the `gh` executable is available; worktree lanes SHALL follow the `git-worktrees` capability.
**Milestone:** M3

#### Scenario: Last-turn scope
- **GIVEN** a turn that edited two files and committed one of them
- **WHEN** the user selects `last-turn`
- **THEN** the pane SHALL show both files' changes and offer no stage or revert actions

#### Scenario: Revert needs confirmation
- **WHEN** the user clicks Revert on a modified file
- **THEN** the pane SHALL ask for confirmation before discarding the change

### Requirement: Memory graph
`/journey` (aliases `/learning`, `/memory-graph`) and the palette entry SHALL open an interactive graph of the profile's skills and memories laid out over time with a playback scrubber and filters `all`, `used` and `learned`; editing or deleting a node SHALL go through the same memory and skill RPCs as any surface (deleting a skill archives it), changes SHALL apply to live sessions only from their next session, and Share SHALL export only a layout code containing no memory or skill text.
**Milestone:** M3

#### Scenario: Share code is content-free
- **WHEN** the user shares the graph layout
- **THEN** the exported code SHALL contain node positions and ids only and no memory text

#### Scenario: Deleting a memory mid-session
- **GIVEN** an open chat
- **WHEN** the user deletes a memory node
- **THEN** the open chat's memory snapshot SHALL be unchanged and new sessions SHALL not include the memory

### Requirement: HUD mode
`CommandOrControl+Shift+H` SHALL detach the focused chat into a chrome-free always-on-top floating bar with the live transcript and a composer and toggle it back; the bar SHALL be movable by press-and-hold, resizable from its edges, resettable to default size, snappable to the pointer by the global shortcut `desktop.hud.snap_hotkey` (default `CommandOrControl+Shift+G`), and summonable by the tap gesture when `desktop.hud.tap_to_summon = true` (default false); when the user submits from the HUD, the metadata of the window underneath it (app, title, bounds, never pixels) SHALL be attached as context, and features the platform support matrix marks unavailable (for example placement on native Wayland) SHALL be disabled with their reason.
**Milestone:** M3

#### Scenario: Asking about the app underneath
- **GIVEN** the HUD parked over a browser window titled `Invoice 42`
- **WHEN** the user asks "summarise this"
- **THEN** the submitted turn SHALL carry the window metadata naming `Invoice 42` and no screenshot

#### Scenario: Snap on native Wayland
- **GIVEN** a native Wayland session
- **WHEN** the user opens HUD settings
- **THEN** Snap to pointer SHALL be disabled with the reason `the compositor controls window placement`

### Requirement: Management panes
The desktop SHALL provide management panes for Skills (installed skills with enable toggles and the shipped optional catalog with one-click install), Scheduled jobs (an `All profiles` view whose run history and actions always target the job's owning profile), Profiles, Messaging, Agents and a Command Center listing every running session, pending approval and active subagent across the connection's profiles with open, stop and answer actions; config-backed settings pages SHALL target the profile chosen in an `Applies to` selector that resets when the active profile changes and is hidden with fewer than two profiles.
**Milestone:** M3

#### Scenario: Pausing another profile's job
- **GIVEN** the Scheduled jobs pane showing all profiles while `work` is active
- **WHEN** the user pauses a job owned by `home`
- **THEN** the request SHALL carry `profile: "home"`

### Requirement: Messaging pane shows saved versus running state
The Messaging pane SHALL show each adapter of the selected profile in exactly one state from the daemon (`not_configured`, `saved` (credentials stored but disabled), `pending_restart` (configuration changed since the adapter started), `connecting`, `connected`, `error` with the reason, or `daemon_stopped`), SHALL keep a Restart adapter banner after any credential save, clear or enable toggle until the adapter has actually restarted, and SHALL keep the banner with a Retry action when the restart fails.
**Milestone:** M3

#### Scenario: Saved but not started
- **GIVEN** a Slack token saved while Slack is disabled
- **WHEN** the user opens Messaging
- **THEN** Slack SHALL show `saved`, not `connected`

### Requirement: Telegram quick setup by QR
The Messaging pane (and the dashboard Channels page) SHALL offer Create with QR for Telegram, which requests a pairing from `gateway.platforms.telegram.options.setup_service_url` (default the project's hosted onboarding service), shows the returned QR code and deep link, polls every 2 s for at most 180 s, validates the returned bot token format, stores the token in the profile's secrets store, adds the returned owner user id to the Telegram allowlist, and restarts only the Telegram adapter; cancelling SHALL stop polling and ignore late results, and expiry SHALL leave configuration unchanged with `Telegram setup timed out; nothing was saved`.
**Milestone:** M3

#### Scenario: Successful QR setup
- **WHEN** the user scans the code and confirms in Telegram within 180 s
- **THEN** the token SHALL be stored as a secret, the user's Telegram id SHALL be the only allowlisted user, and the Telegram adapter SHALL reach `connected`

#### Scenario: Cancelled setup
- **WHEN** the user closes the dialog while polling
- **THEN** a result arriving afterwards SHALL be discarded and no secret SHALL be written

### Requirement: Repository discovery for the Projects sidebar
The desktop Projects sidebar SHALL list explicit projects, repositories inferred from sessions and the repositories discovered by `projects` › Repository discovery (which owns the scan, its `projects.repo_scan_*` keys and cache rules), offering one-click project creation for a discovered repository and removing discovered rows as soon as scanning is disabled.
**Milestone:** M3

#### Scenario: Scanning disabled
- **GIVEN** a profile with discovered repositories
- **WHEN** the user sets `projects.repo_scan_enabled = false`
- **THEN** discovered rows SHALL disappear while projects created explicitly SHALL remain

### Requirement: Fonts, shortcuts, palette and profile files
The desktop SHALL apply `desktop.font_family` (chat and UI, layered in front of the theme's stack; empty uses the theme's font) and `desktop.terminal_font_family` (terminal pane; empty uses the bundled monospace font), open the command palette with `CommandOrControl+K` or `CommandOrControl+P` to navigate pages, settings, sessions by title or id and actions, let the user rebind every shortcut including plugin-contributed ones with duplicate chords flagged as conflicts, zoom the interface in half steps, and export or import a profile file through the `configuration-and-profiles` profile export format with secrets stripped and the desktop appearance included as an optional overlay.
**Milestone:** M3

#### Scenario: Conflicting shortcut
- **WHEN** the user binds `CommandOrControl+G` to New session while it opens the review pane
- **THEN** the shortcuts panel SHALL flag both bindings as conflicting until one is changed

#### Scenario: Exported profile has no keys
- **WHEN** the user exports profile `work` from the palette
- **THEN** the archive SHALL contain no secret values

### Requirement: Connection registry
The desktop SHALL keep a registry of connections of kinds `local` (app-managed, exactly one, not removable), `remote` and `ssh`, each with a unique case-insensitive name of at most 64 characters, deduplicated on normalized URL (trimmed, trailing slashes removed, lowercased) for `remote` and on `user@host:port` plus remote profile for `ssh`, one marked primary (removing it falls back to `local`); the registry file in the app's data directory SHALL hold names, kinds, URLs and hosts only, credentials SHALL live in the OS keychain and never reach the webview or plugins, and Test SHALL probe both the HTTP health endpoint and a WebSocket handshake.
**Milestone:** M3

#### Scenario: Duplicate URL
- **GIVEN** a remote connection to `https://box.example/`
- **WHEN** the user adds another remote connection to `https://BOX.example`
- **THEN** saving SHALL fail with `a connection to https://box.example already exists (Homelab)`

#### Scenario: Plugin lists connections
- **WHEN** a plugin calls `host.connections()`
- **THEN** it SHALL receive names, kinds and the primary id and no credential material

### Requirement: Remote connections are authenticated and encrypted
A `remote` connection SHALL target the dashboard listener of another Prometheus daemon and SHALL require `https` unless the host is loopback; the TLS certificate SHALL verify against the system roots or, for a self-signed server, against a SHA-256 public-key pin the user confirmed on first connect after comparing the fingerprint the server prints with `prometheus dashboard --fingerprint`, and a pin mismatch SHALL latch an error naming both fingerprints without retrying; the desktop SHALL authenticate with a device credential obtained by pairing or through the server's configured `dashboard.auth.mode`, and SHALL attach with the role the server grants that principal.
**Milestone:** M3

#### Scenario: Plain HTTP to a LAN host
- **WHEN** the user saves a remote connection to `http://192.168.1.5:9120`
- **THEN** saving SHALL fail with `remote connections require https (or an ssh connection)`

#### Scenario: Certificate changed
- **GIVEN** a pinned self-signed server
- **WHEN** the server presents a different key
- **THEN** the desktop SHALL stop retrying and show both fingerprints with a Re-pin action

### Requirement: SSH connections reuse the local ticket model
An `ssh` connection SHALL run the system `ssh` client (or `desktop.ssh_path` on Windows) with strict host-key checking, showing the host-key fingerprint for confirmation on first use and latching an error without retrying when the key changes; it SHALL run `prometheus daemon ensure --json` on the remote host (or the configured remote binary path), forward a local loopback port to the reported remote loopback endpoint, and obtain one-use dial tickets by running `prometheus daemon ticket --json` over the same SSH session, so no network-exposed listener is needed on the remote host; tunnels SHALL open only when an agent on that connection is opened, never on hover or enumeration.
**Milestone:** M3

#### Scenario: Host key changed
- **GIVEN** an ssh connection whose host was reinstalled
- **WHEN** the desktop dials it
- **THEN** it SHALL show `The host key has changed since you last connected` with the new fingerprint and SHALL not retry until the user acts

#### Scenario: Remote binary missing
- **WHEN** Test runs on a host where `prometheus` is not on the non-interactive PATH and no path is configured
- **THEN** Test SHALL report `Prometheus is not installed on the remote host` and suggest setting the remote binary path

### Requirement: Gateway switching and scoping
The desktop SHALL organise navigation as connection → profile → sessions, keep one protocol connection per `(connection, profile)` pair so background sessions keep streaming while another connection is shown, scope the session list, messaging, scheduled jobs, settings, files and memory to the active `(connection, profile)`, route approval answers to the daemon that owns the session, open on launch the primary connection or, when `Open on launch: last used` is chosen, the last connection that connected successfully, and allow renaming and reordering connection and profile groups in the sidebar as local display state only.
**Milestone:** M3

#### Scenario: Approval from a background connection
- **GIVEN** the window shows connection `Laptop` while a session on `Homelab` asks for approval
- **WHEN** the user answers from the notification card
- **THEN** `approval.respond` SHALL be sent on the `Homelab` connection

#### Scenario: Failed switch is not remembered
- **GIVEN** `Open on launch: last used`
- **WHEN** a switch to `VPS` fails and the app restarts
- **THEN** the app SHALL open on the last connection that succeeded

### Requirement: Update every connected instance
When more than one connection is registered, Update all instances SHALL update the local install through the app's updater and ask every other reachable connection to update itself through `daemon.update` (allowed only for principals the server grants `admin`), running in parallel and reporting each instance's outcome separately; an instance whose install is owned by a package manager or image SHALL report the owner's refusal on its row without failing the batch, and the desktop app itself SHALL update last.
**Milestone:** M3

#### Scenario: One instance is unreachable
- **GIVEN** three registered connections, one of them offline
- **WHEN** the user clicks Update all instances
- **THEN** the other two SHALL update and the offline row SHALL show `unreachable`

### Requirement: Agents across connections
Every profile on every registered connection SHALL appear as an agent in a union roster available to Bot Mode and plugins (`host.agents()`, `host.ensureAgent(connection_id, profile)`, `host.warmAgent(connection_id, profile)`), with handles disambiguated as `@<profile>-<connection name>` only when a profile name exists on several connections; enumeration SHALL use each connection's HTTP API without opening sockets, an unreachable connection SHALL be reported on its rows without breaking the roster, opening an agent SHALL run it on its own daemon, and mentions and delegation SHALL never cross connections implicitly (cross-machine messaging belongs to `agent-interop`).
**Milestone:** M3

#### Scenario: Same profile name on two machines
- **GIVEN** profile `research` on connections `Homelab` and `Laptop`
- **WHEN** the roster renders
- **THEN** the handles SHALL be `@research-homelab` and `@research-laptop`

#### Scenario: Mention does not cross machines
- **GIVEN** `@research-homelab` typed in a chat on `Laptop`
- **WHEN** the message is submitted
- **THEN** no turn SHALL start on `Homelab` unless an `agent-interop` peer link authorises it

### Requirement: Desktop UI toolset contract
The `desktop_ui` toolset SHALL contain exactly `react_to_message`, `read_terminal`, `close_terminal`, `desktop_preview`, `drive_preview`, `annotate_preview`, `read_window_below`, `focus_pane`, `gui_tour`, `show_tip` and `set_theme`, SHALL be folded in only for `desktop` sessions, and SHALL reach the window through server-to-client requests (`ui.*`) answered by a driver attachment that advertised the `desktop_ui` feature, with a 30 s deadline (300 s for picker and tour requests); a call with no such attachment SHALL return `{"success": false, "error": "<tool> needs an open Prometheus Desktop window"}`, user opt-outs that hide tools (reactions off, tours off, tips off) SHALL change the toolset only from the next session, and effects that move focus SHALL apply only in the focused window so a background turn never steals focus.
**Milestone:** M3

#### Scenario: No window attached
- **GIVEN** a desktop session whose window was closed
- **WHEN** the agent calls `read_terminal`
- **THEN** the result SHALL be the structured error naming `read_terminal` and the turn SHALL continue

#### Scenario: Turning tips off mid-session
- **WHEN** the user disables in-app tips during a session
- **THEN** the session's tool list SHALL still contain `show_tip` and new sessions SHALL not

### Requirement: Agent reactions
`react_to_message {emoji, message_id?, messages_back?}` SHALL set the agent's single reaction on a message of the current session (default the latest user message, `messages_back = n` the n-th earlier user message), replacing any previous agent reaction and retracting it when `emoji` is empty, emit `message.reaction` to attached clients, return `{success, message_id, reactions}`, and SHALL be in the toolset only when the user enabled message reactions (default off).
**Milestone:** M3

#### Scenario: Reacting to an earlier message
- **WHEN** the agent calls `react_to_message {emoji: "😂", messages_back: 1}`
- **THEN** the reaction SHALL be set on the user message before the latest one

#### Scenario: Foreign message id
- **WHEN** the agent passes a `message_id` from another session
- **THEN** the result SHALL be `{"success": false, "error": "message <id> is not part of this conversation"}`

### Requirement: Terminal pane tools
`read_terminal {start_line?, count?}` SHALL return `{total_lines, start, end, viewport_rows, cursor_row, text}` for the focused terminal tab beside the chat (the visible screen when no arguments are given, scrollback paged with `start_line` from 0 = oldest, at most 2000 lines per call) with secret redaction applied, and `close_terminal {process_id}` SHALL close only the read-only tab that mirrors a background process without stopping the process, whose output keeps buffering.
**Milestone:** M3

#### Scenario: Closing a mirror tab
- **GIVEN** a background dev server mirrored in a terminal tab
- **WHEN** the agent calls `close_terminal`
- **THEN** the tab SHALL close and the dev server SHALL keep running

### Requirement: Preview pane tool
`desktop_preview {action: open|close|read, url?, label?, start?, count?}` SHALL open a web URL, a loopback dev server or a workspace file in the preview rail of the focused window (web URLs subject to the same URL safety policy as web tools, loopback dev servers allowed), close the whole rail or one tab, and read the visible text of a browser tab as `{kind, url, title, text, start, end, total_chars}` paged by character offset with at most 20000 characters per read; a file tab SHALL answer identity only.
**Milestone:** M3

#### Scenario: Blocked private address
- **WHEN** the agent opens `http://169.254.169.254/latest/meta-data`
- **THEN** the call SHALL be refused under the URL safety policy and no page SHALL load

### Requirement: Driving the preview page
`drive_preview {action, ref?, selector?, text?, submit?, key?, allow_shortcut?, amount?, to?, max?, full?}` SHALL act on the preview page with actions `elements`, `click`, `hover`, `type`, `scroll`, `press`, `strobe`, `back`, `forward` and `reload`, addressing elements by stable refs from `elements` (refs survive re-renders and are reported stale after navigation) and answering later actions with deltas (`added`, `changed`, `removed`, `rebound`); a printable `press` on the page body SHALL be refused unless `allow_shortcut = true`, typing into a password field SHALL require an approval, and navigation it causes SHALL follow the preview URL safety policy.
**Milestone:** M3

#### Scenario: Stale ref
- **GIVEN** refs from an `elements` call before the page navigated
- **WHEN** the agent clicks one of them
- **THEN** the result SHALL say the ref is stale and ask for a new `elements` call

#### Scenario: Typing a password
- **WHEN** the agent types into an `input[type=password]`
- **THEN** an approval request SHALL be raised before any character is entered

### Requirement: Annotating the preview page
`annotate_preview {action: add|hold|remove|clear, ref?, selector?, label?}` SHALL draw lasting outlines on preview elements (`add` with an optional short label, `hold` outlining every visible element with its ref, `remove` one, `clear` all), marks SHALL follow their element on scroll and be cleared by navigation, and `add` or `remove` without a ref or selector SHALL return an error naming the missing target.
**Milestone:** M3

#### Scenario: Navigation clears marks
- **GIVEN** two annotations on the page
- **WHEN** the page navigates
- **THEN** both marks SHALL be gone

### Requirement: Reading the window below
`read_window_below {}` SHALL return metadata of the OS window directly beneath the desktop window as `{window: {app, title, bounds, id}, frontmost, platform}` and SHALL never capture pixels; when the OS withholds titles without a permission (for example macOS screen recording) `title` SHALL be empty with a `note` naming the permission, and where enumeration is unavailable it SHALL return `{error, platform}` naming what would fix it.
**Milestone:** M3

#### Scenario: Missing macOS permission
- **GIVEN** the app lacks the screen-recording permission
- **WHEN** the agent calls `read_window_below`
- **THEN** the result SHALL include the app name, an empty title and a note naming the permission

### Requirement: Pane focus, tours and tips
`focus_pane {pane: chat|files|terminal|review|sessions}` SHALL reveal that pane in the focused window; `gui_tour {action: targets|show|start|next|prev|stop, surface: app|preview, selector?, title?, text?, side?, steps?, step_index?, preset: quick|full}` SHALL list stable targets, highlight one step or run a paged tour (the built-in tour when `start` has no steps) and SHALL be in the toolset only while tours are enabled; `show_tip {text, selector, title?, side?}` SHALL show one arrow bubble replacing the previous one, without waiting for the user, and SHALL be in the toolset unless the user turned tips off.
**Milestone:** M3

#### Scenario: Unknown pane
- **WHEN** the agent calls `focus_pane {pane: "inbox"}`
- **THEN** the result SHALL be an error listing the five allowed panes

#### Scenario: Preset and steps together
- **WHEN** the agent calls `gui_tour {action: "start", preset: "quick", steps: [...]}`
- **THEN** the result SHALL be `pass steps or preset, not both`

### Requirement: Appearance tool
`set_theme {accent?, mode?, layout?, picker?}` SHALL apply an accent (`mono`, `green`, `cyan`, `violet`, `pink`, `red`, `orange`, `theme` or `#rrggbb`), a mode (`light`, `dark`, `system`) and a layout preset or saved layout id to the machine UI preferences and return what the window actually applied, or with `picker: accent|layout` alone show a picker card and wait up to 300 s for the user's choice; it SHALL never change the system prompt, toolset or any profile configuration.
**Milestone:** M3

#### Scenario: Unknown layout
- **WHEN** the agent calls `set_theme {layout: "cockpit"}`
- **THEN** the result SHALL be an error listing the available presets and saved layouts and nothing SHALL change

### Requirement: Project tool
Desktop sessions SHALL receive the `project` tool defined by `projects` › Project tool for GUI sessions (Hermes' `desktop_project` maps onto it), and the Projects sidebar SHALL move a session under its new project as soon as a `create` or `switch` call takes effect.
**Milestone:** M3

#### Scenario: Switching project
- **GIVEN** projects `api` and `web`
- **WHEN** the agent calls `project {action: "switch", project: "web"}`
- **THEN** the session's workspace SHALL become the `web` project's primary folder and the sidebar SHALL show the session under `web`

### Requirement: First-run onboarding chat
On first launch the desktop SHALL open an onboarding chat whose session alone receives the `onboarding` toolset containing `setup_beat {beat, question?, value?, skip?, suggest?, options?}`, which shows the card for the beats `name`, `theme`, `layout`, `connectors`, `plugins` (at most 5 suggested catalog plugins), `tour` and `build` in that order, waits up to 300 s for the user, records the answer in the session's onboarding record and returns it with a reminder of the next beat; only the user's Skip setup button SHALL end onboarding early, and installing a suggested plugin SHALL still go through the normal consent flow.
**Milestone:** M3

#### Scenario: Regular chats lack the tool
- **WHEN** the user opens a normal chat after onboarding
- **THEN** its tools SHALL NOT include `setup_beat`

#### Scenario: Card deadline
- **WHEN** the user leaves the `theme` card unanswered for 300 s
- **THEN** `setup_beat` SHALL return that the beat timed out and the defaults stay in place

### Requirement: Agent-to-agent messages from bot chats
The `message_agent {target, message}` tool SHALL be present only in a Bot Mode bot's canonical bot chat when `bot_mode.protocol = true` (default), SHALL validate `target` against the live roster (a local profile name, or `<peer>/<agent>` for an `agent-interop` peer), SHALL reject messages over 16000 characters, prefix the recipient's copy with the sender's attribution server-side, deliver it to the recipient's bot chat asynchronously and return `{status: "queued", delivery_id}` immediately, with the reply or the failure arriving later as a completion notification; a message whose delivery chain already holds 4 agent-to-agent hops SHALL be refused with `agent message hop limit (4) reached`.
**Milestone:** M3

#### Scenario: Unknown teammate
- **WHEN** a bot calls `message_agent {target: "reseacher", ...}`
- **THEN** the result SHALL be an error listing the roster names closest to `reseacher` and nothing SHALL be delivered

#### Scenario: Forged call outside a bot chat
- **WHEN** a regular session emits a `message_agent` call
- **THEN** the dispatcher SHALL refuse it as an unknown tool for that session

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
The desktop SHALL accept an ensure endpoint only when its origin is `http` on `127.0.0.1` or `[::1]` with an explicit port and no credentials, path, query or fragment, and it carries a non-empty `instance_id`, an `rpc_protocol` whose major the desktop speaks, and the `session-attach` feature; otherwise it SHALL refuse with `Invalid local gateway endpoint: <reason>`.
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
The desktop SHALL register the `prometheus://` URL scheme and handle exactly `prometheus://session/<id>` (open that session in the profile that owns it) and `prometheus://new?profile=<name>` (open a draft); any other path SHALL be ignored with a WARN log, and a deep link SHALL never submit input or answer a request.
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

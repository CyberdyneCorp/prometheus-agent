# Computer use

The [`computer_use`](../reference/tools.md#computer_use) tool lets any tool-capable model drive a desktop:
capture the screen, click, type, scroll, drag and press keys. It speaks
MCP over stdio to a pinned external driver, `cua-driver`, which sends
input to the target window in the background: the user's cursor does not
move, windows are not raised and desktops are not switched.

## Enabling it

`computer_use` is in the `core` toolset (not in `coding`, the API server
or the other non-interactive defaults). It is offered only when the
backend is ready: the driver is installed or can be installed lazily, and
the platform prerequisites pass. `prometheus tools list` shows the reason
when it is not, for example
`unavailable:cua-driver is not installed and lazy installs are disabled`.

```sh
prometheus computer-use install          # the pinned cua-driver runtime
prometheus computer-use status           # driver, version, mode, overlay
prometheus computer-use doctor           # the per-check health matrix
prometheus computer-use permissions status
prometheus computer-use permissions grant   # macOS: opens the settings pane
```

Without a driver the doctor says how to get one, and `unrestricted` is
refused as a config value (`prometheus config validate` exits 78 on a
file that sets it):

```console
$ prometheus computer-use doctor # exits 69
<...>prometheus computer-use install<...>
$ prometheus config set computer_use.permission_mode unrestricted # exits 2
<...>unknown value "unrestricted" (allowed: "standard", "bounded")
```

`computer_use.driver_path` uses your own driver binary instead; it is
never repaired or replaced. A managed driver whose version falls outside
the supported contract (0.20 or newer) is repaired once per session.

## Actions

`action` is one of `capture`, `click`, `double_click`, `right_click`,
`middle_click`, `drag`, `scroll`, `type`, `key`, `set_value`, `wait`,
`list_apps`, `list_windows` and `focus_app`. Actions target an `element`
index from the latest capture or `x`/`y` coordinates, optionally an
`app`/`window`; `capture_after = true` captures again after the action.
An element the latest capture does not have answers
`stale element <n>; capture again`.

Captures come in three modes: `som` (default; a screenshot with numbered
elements), `vision` (pixels only) and `ax` (elements only). A model that
cannot see images is offered only `ax`. At most 100 elements reach the
model; the full list goes to a session cache file named in the result.
Images are capped at `computer_use.max_image_dimension` (1456 px) and kept
under `<profile>/cache/computer-use/` (the newest 20). On a messaging
platform the agent sends one with a `MEDIA:` line only when you ask to see
the screen.

Screenshots cost context: each counts as 1,500 tokens. When a request
would exceed the provider's image limit (100 images of at most 2,000 px,
otherwise 20) or 24 MB of images, the oldest screenshots are replaced by
`[screenshot removed to save context]` in whole batches; your own
attachments are never removed, and below the limit nothing earlier is
rewritten (the prompt cache holds).

## Approvals and hard blocks

`capture`, `wait`, `list_apps` and `list_windows` never ask. Every other
action asks under `computer_use:<action>:<background|foreground>`, so a
`session` or `always` grant covers one action in one delivery mode: a
background grant never covers `delivery_mode = "foreground"` (or
`focus_app` with `raise_window`). Cron, single-query and unattended runs
refuse them unless `approvals.<context>_mode = "approve"` or yolo is on.

Refused in every mode, yolo included: shortcuts that empty the trash,
force-delete, lock the screen or log out, and typed text matching the
hardline command patterns (`curl ... | sh`, `rm -rf /`, fork bombs).

## Permission modes

The driver's runtime is launched with a fixed permission mode:

- `standard` (default): the driver's actions pass Prometheus' approvals;
- `bounded`: `computer_use.permission_mode = "bounded"` with a reviewed
  `computer_use.capability_manifest`; a missing or unreadable manifest
  fails the session at start;
- `unrestricted`: only while `/yolo` is on, for that session alone.
  `unrestricted` is not a config value (validation exits 78). Turning yolo
  off stops the session's unrestricted runtime.

Bounded and unrestricted runtimes are private to their session. On macOS
they start only through `CuaDriver.app` whose code signature carries the
official identifier and team; `computer_use.allow_unsigned_driver = true`
admits a local driver build.

## Which machine is driven

The driver acts where the session's screen lives (`bot_desktop.placement`):
this host for the `local` backend, the desktop container
(`prometheus-sandbox:<version>-desktop`, which carries the driver) for the
container backend, the SSH host for the SSH backend. Backends that cannot
host a display are refused unless `bot_desktop.placement = "gateway"`. A
person holding the bot screen (`screen.takeover`) blocks every action,
captures included.

## Platform prerequisites

- macOS: Accessibility and Screen Recording granted to CuaDriver
  (`permissions status` reports `screen_recording: missing` and exits 1).
- Linux: an X11 display (or Wayland with `computer_use.native_wayland`)
  and the AT-SPI bus of a desktop session; the bot screen provides both.
- Windows: an interactive session, or `computer_use.autostart = true` for
  a per-logon driver task.

The driver's telemetry is off unless `computer_use.driver_telemetry =
true`. Its agent cursor overlay is labelled per session; `auto` turns it
off on macOS, headless or WSL Linux, containers and Linux X11
(`computer_use.overlay = "on"|"off"` overrides).

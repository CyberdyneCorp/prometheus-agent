# Browser automation

[`browser_exec`](../reference/tools.md#browser_exec) (toolset `browser`,
part of `core`) lets the model drive a real browser by running Python 3.
It is offered only in sessions that also have the `terminal` toolset.
[`browser_cdp`](../reference/tools.md#browser_cdp) and
[`browser_dialog`](../reference/tools.md#browser_dialog) drive a browser
you connect (see [Your own browser](#your-own-browser)).

```console
$ prometheus tools explain browser_exec
<...>
exposure = exposed
$ prometheus config set browser.provider nope
$ prometheus config validate # exits 78
<...>browser.provider: unknown browser provider 'nope' (expected local, browserbase, browser-use, firecrawl, subscription)
```

## What a call does

Each call runs the model's code in a fresh Python interpreter (the managed
`cpython` runtime, or `python3` on the host) under the session's OS
sandbox, with these helpers pre-imported:

| Helper | Does |
|---|---|
| `new_tab(url=None)` | opens a tab (on `url`) and makes it current |
| `goto_url(url)` | navigates the current tab |
| `wait_for_load(timeout=30)` | waits until the page has loaded |
| `page_info()` | URL, title, visible text, open tabs and pending dialogs |
| `js(expression)` | evaluates JavaScript in the page and returns its value |
| `fill_input(selector, text)` | types into the element matching a CSS selector |
| `click_at_xy(x, y)` | clicks at a viewport position |
| `capture_screenshot(path=None)` | saves a PNG in the workspace and returns its path |
| `cdp(method, params=None)` | sends a raw DevTools command (page domains `Page`, `Runtime`, `DOM`, `DOMStorage`, `Emulation`, `Input`, `Network` go to the current tab, the rest to the browser) |
| `ensure_real_tab()` | makes sure a page tab is current |

A failing helper raises `BrowserError`. The result carries what the code
printed (`output`), the traceback if it failed (`error`), the `workspace`
directory and the screenshot paths. Screenshots are attached as images
when the model accepts image input. The browser (tabs, cookies, the
current page) and the workspace directory persist across calls; Python
variables do not. `timeout_secs` defaults to 300 (1 to 1800). A named
`session` gets its own browser and workspace.

`cdp` refuses `Browser.close`, `Browser.crash` and closing a tab the
session did not open.

## Which browser

The first call of a session key resolves a browser, in this order:

1. an explicit DevTools endpoint: the session's `/browser connect`, then
   `browser.cdp_url`;
2. the cloud browser provider named by `browser.provider` (default
   `local`, see [Cloud browsers](#cloud-browsers); any other name fails
   `prometheus config validate` with exit 78). When the provider
   fails, a local browser is used and results carry
   `fallback_from_cloud: true`; when both fail the error names both
   reasons;
3. a local Chromium: `browser.executable_path` when set, else the managed `chromium` runtime, else a
   Chrome, Chromium, Brave or Edge installed on the host (not on Windows,
   see Isolation), else the runtime
   installed on first use when lazy installs are allowed
   (`runtimes.allow_lazy_install`, part of the managed runtime store);
   otherwise the call fails with `chromium is not installed and lazy
   installs are disabled: run 'prometheus runtimes install chromium'`
   (or, for a runtime listed in `runtimes.exclude`, `chromium is excluded
   by runtimes.exclude: run 'prometheus runtimes exclude remove chromium'
   to allow it`). A one-shot run installs without asking; the line
   interface asks `Install chromium <version> (<size>)? [Y/n]` first
   (Enter installs, `n` declines).

The interpreter of [`browser_exec`](../reference/tools.md#browser_exec) resolves the same way through the
`cpython` runtime (the managed one, else `python3`/`python` on `PATH`,
else a lazy install).

When a session starts, the browser tools are offered only if these
runtimes can be had without an install the policy forbids: with Chromium
(for a local browser) or CPython missing, `tools list` and the session's
surface show them as `unavailable:lazy installs disabled` or
`unavailable:excluded`, and the model cannot call them.

Each local browser gets a throwaway profile under
`<profile>/cache/browser/profiles/` and is recorded under
`<profile>/cache/browser/records/`, so a later process reaps the browser
of an owner that crashed (after checking its pid, start time and profile
directory). On Unix each local browser leads its own process group, so
closing or reaping it also kills its helper processes (zygote, renderers,
crash handler) before its directories are removed. A browser that died is replaced on the next call. Sessions
idle longer than `browser.inactivity_timeout_secs` (default 120, read when
checked) are closed.

### Isolation

On macOS and Linux a local browser runs under the OS sandbox of
`security.sandbox` (Seatbelt; Landlock and seccomp). It reads the system
toolchain and its own installation (the whole `.app` bundle on macOS; on
Linux also `/proc` and `/sys`), writes only its throwaway profile directory
and a private temp directory (removed with it), and connects only to the
session's egress proxy: Seatbelt allows loopback to the proxy's port, and
on Linux the browser runs in its own network namespace whose only way out
is the proxy. Every connection it makes therefore follows the session's
`/egress` policy, and credential stores stay unreadable. DevTools runs over
a pipe (`--remote-debugging-pipe`), so the browser listens on no port that
another local process could drive. Chromium's own sandbox cannot start
inside the OS sandbox (nested Seatbelt profiles and user namespaces are
refused), so the browser runs with `--no-sandbox` while the OS sandbox
confines its whole process tree. A host whose sandbox is degraded follows
`security.sandbox.allow_degraded` like any other confined child.

On Windows the browser runs in an `AppContainer` (with `--no-sandbox`,
for the same reason): it writes only its profile and temp directories, has
no network capability, and reaches the egress proxy through a relay inside
its own container (its `--proxy-server` address is pointed at the relay).
DevTools uses two anonymous pipes the browser inherits and finds through
`--remote-debugging-io-pipes`, so nothing listens on a port there either.
Google Chrome, Microsoft Edge and other full Chromium builds cannot start
there (their crash handler opens a named pipe outside the container's pipe
namespace before reading any switch), so Windows never picks a host
browser: a local browser is always the managed `chromium` runtime
(`chrome-headless-shell`, which starts no crash handler), installed on
first use like any runtime. Windows on Arm runs its x64 build under
emulation (Chrome for Testing publishes no arm64 build).

A session browses with a throwaway profile: without the
`browser.real_profile` grant in `grants.toml` the user's real browser
profile is never used (see [Your real profile](#your-real-profile)).

### Local browser options

| Key | Default | Does |
|---|---|---|
| `browser.executable_path` | empty | the browser binary; empty uses the managed runtime, then a host browser |
| `browser.user_data_dir` | empty | a profile directory kept between browsers (relative paths are under the profile); empty uses a throwaway one |
| `browser.chromium_args` | `[]` | extra switches |
| `browser.launch_timeout_secs` | 30 | bound on a browser start |
| `browser.headed` | false | open a visible window |

A headed browser is not closed between turns (idle sessions are still
closed after `browser.inactivity_timeout_secs`) and stays confined: on
macOS it may also reach the window server, on Linux the X11 or Wayland
socket. On a host without a display (no `DISPLAY` or `WAYLAND_DISPLAY` on
Linux) it runs headless, and the first result says `browser.headed is set
but no display is available: the browser runs headless`. Headed mode does
not apply to cloud browsers. Switches that turn off Chromium's own sandbox
(`--no-sandbox`, `--disable-setuid-sandbox`...) need
`security.sandbox.allow_degraded = ["chromium_sandbox"]`; switches that
turn off web security (`--disable-web-security`,
`--disable-site-isolation-trials`, `--allow-running-insecure-content`)
need `"web_security"`. Without the entry the browser does not start:

```text
sandbox unavailable: browser.chromium_args '--no-sandbox' turns off Chromium's own sandbox; set security.sandbox.allow_degraded = ["chromium_sandbox"] to run without it
```

With it, like any sandbox downgrade, every browser start logs a WARN, the
session banner shows `browser: degraded:chromium_sandbox`, and each session
records one `security.downgrade` trajectory event and audit record per
switch. Switches Prometheus sets itself (profile directory,
DevTools transport, proxy, `--headless`) are refused. A browser on a kept
profile is asked to close before it is ended, so it writes what it holds.

## Cloud browsers

Set `browser.provider` to one of the first-party providers; a provider
key in the secrets store never selects one by itself.

| Provider | Secrets | Hosts |
|---|---|---|
| `browserbase` | `BROWSERBASE_API_KEY`, `BROWSERBASE_PROJECT_ID` (`BROWSERBASE_BASE_URL` overrides the API) | `api.browserbase.com`, `*.browserbase.com` |
| `browser-use` | `BROWSER_USE_API_KEY` | `api.browser-use.com`, `*.browser-use.com` |
| `firecrawl` | `FIRECRAWL_API_KEY` (`FIRECRAWL_API_URL` overrides the API) | `api.firecrawl.dev`, `*.firecrawl.dev` |
| `subscription` | the Nous sign-in (`prometheus auth add nous`) | `*.nousresearch.com`, `*.browser-use.com` |

A provider talks only to the hosts it declares (plus an API URL you set);
a DevTools URL on another host is refused. Its session is released when
the browser session ends. Browserbase sessions ask for keep-alive and
proxies and are retried without them on a plan that lacks them (HTTP
402). `prometheus browser status` lists live sessions with their
provider.

With a cloud provider and `browser.auto_local_for_private_urls = true`
(default), a navigation of [`browser_exec`](../reference/tools.md#browser_exec) to a private, loopback or LAN
address (the private ranges, `localhost`, `*.local`, `*.lan`,
`*.internal`) opens in a local sidecar browser, so the provider never
receives the URL; a public URL returns to the cloud browser, and the
result says `local_sidecar` while the sidecar holds the page. The sidecar
is a local browser like any other: its navigations still follow
[Safety](#safety). With the switch off, such a navigation is refused with
`Blocked: URL targets a private or internal address` unless the egress
policy allows the address. Cloud metadata endpoints are refused on every
route.

## Your own browser

In the line interface and the TUI:

- `/browser connect [<ws-or-http-url>]` binds the session to a browser's
  DevTools endpoint. Without a URL it uses `http://127.0.0.1:9222`, and if
  nothing listens there it starts Chrome, Brave, Edge or Chromium with
  `--remote-debugging-port=9222` and a dedicated profile under
  `<profile>/cache/browser/connect/` (Chrome 136+ refuses remote debugging
  on the default profile). It also offers [`browser_cdp`](../reference/tools.md#browser_cdp) and
  [`browser_dialog`](../reference/tools.md#browser_dialog) from then on, to this session only, which rebuilds the
  session's tools (its prompt cache starts over) unless `browser.cdp_url`
  already offers them. Other sessions of the same process (the daemon's
  included) do not get them from your connection.
- `/browser status` shows the endpoint, the browser version and the open
  targets.
- `/browser disconnect` detaches without closing your browser; the next
  call uses `browser.provider` again.

Messaging platforms answer `/browser` with `/browser is not available
here`. A connected browser is yours: it is not confined, and ending the
session never closes it.

`browser_cdp(method, params?, target_id?, frame_id?, timeout_secs?)`
sends one raw DevTools command: page-level domains (`Page`, `Runtime`,
`DOM`, `Emulation`, `Input`, `Network`) need `target_id`, a `frame_id` of
an out-of-process frame goes to that frame's own session, `timeout_secs`
defaults to 30 (at most 300), and `Browser.close`, `Browser.crash`,
closing a target the session did not open and any URL parameter the
safety rules refuse (every string under a URL-named key such as `url`,
`frameUrl`, `documentURL` or `urls`, at any depth of `params`) are
refused. `browser_dialog(action, prompt_text?, dialog_id?)`
accepts or dismisses the oldest pending dialog (or the one named) and
answers `no pending dialog` when there is none.

## Your real profile

With `browser.use_real_profile = true` and the `browser.real_profile`
grant, a local session browses on a copy of your default Chromium-family
browser's last used profile (or `browser.real_profile_pin`, a directory
such as `Profile 2`), kept under
`<profile>/cache/browser/real-profile/<browser>/` and re-synced at every
browser launch. Databases are copied with SQLite's online backup; the live
profile is never opened. The browser binary is your browser's, headless
unless `browser.headed`, confined like any local browser. When the copy is
impossible (a database your browser holds locked, a default browser
outside the Chromium family, a pin that does not exist) the session gets a
throwaway profile and its first result carries `real_profile_unavailable`
with the reason. `browser_exec(local=true)` (offered only with this
setting) uses the local browser even when a cloud provider is configured.
Turning the setting off deletes the copy at the next browser use.

`prometheus browser close-profile` ends your browser (after asking, or
with `--yes`) so its databases can be copied. The agent never runs that
command: from its terminal it is blocked under every approval mode
(`BLOCKED (hardline)`). With `browser.real_profile_autoclose = true` it
gets [`browser_close_profile`](../reference/tools.md#browser_close_profile) instead, which does the same from
Prometheus itself (outside the terminal sandbox, which cannot signal your
browser) and asks you every time, YOLO included, never remembering the
answer; cron, API and one-shot runs neither get it nor can approve it.
After it runs, the session's next browser call starts on a fresh copy.
Cookies your browser encrypted with the OS
keychain stay unreadable in the copy: the sandbox denies keychain access.

## Safety

Before every navigation (and any URL parameter of `cdp` and [`browser_cdp`](../reference/tools.md#browser_cdp)):

- a URL holding a secret-shaped value or a secret the session knows (raw
  or percent-decoded) is refused with `Blocked: URL contains what appears
  to be an API key or token.`;
- cloud metadata endpoints are always refused with `Blocked: URL targets a
  cloud metadata endpoint`;
- private and internal addresses are refused with `Blocked: URL targets a
  private or internal address` unless `security.egress.allow_private` or a
  grant allows them.

## Dialogs

JavaScript dialogs follow `browser.dialog_policy`: `must_respond`
(default) leaves them for the model (`page_info()` lists them; answer with
`cdp('Page.handleJavaScriptDialog', {...})`, or [`browser_dialog`](../reference/tools.md#browser_dialog) on a
connected browser) and dismisses them after
`browser.dialog_timeout_secs` (default 300); `auto_dismiss` and
`auto_accept` answer at once. What happened to dialogs during a call is
listed in the result's `dialogs`. A dropped DevTools connection is
re-established; after 5 failures in a row the session is replaced on the
next call.

## Saved logins, cards and addresses

[`browser_vault`](../reference/tools.md#browser_vault) lets the model use
the credential vault without ever seeing a secret. It lists items by
handle (label, kind, origins and identifier only) and fills one into the
current page of the session's browser; the result is just
`{"filled_fields": 1, "origin": "https://github.com"}`.

- A login fills only on a page whose origin is exactly one of its saved
  origins: a `https://github.com` login is refused on
  `https://github.com.evil.example` and nothing is typed.
- Every payment-card fill asks you, each time and in every approval mode
  (yolo included); cron, webhook, API and single-query runs refuse it.
  Addresses fill without asking.
- `enter_code` types the current TOTP code of a login with a seed (on its
  origins only); without a seed it asks you for `Verification code for
  <origin>` with the input hidden. For passkeys or app approvals the model
  is told to ask you to finish on your device.
- `save_login` asks you for the password with the input hidden (the model
  supplies only the identifier); runs with nobody to ask answer
  `prompt_unavailable`.
- Every value filled is replaced by `[REDACTED:vault]` in the session's
  later [`browser_exec`](../reference/tools.md#browser_exec) output, so reading the field back shows nothing.

Manage the vault from the command line (or the desktop and dashboard
settings, over the `vault.*` RPC methods):

```console
$ prometheus vault list
the vault is empty; add an item with prometheus vault add
$ prometheus vault add --login --password hunter2 # exits 2
$ prometheus vault sources
```

`prometheus vault add --login --origin https://github.com --identifier
octocat [--with-totp]`, `--card --holder <name> --expiry MM/YY` and
`--address --name ... --line1 ... --city ... --postal-code ... --country
...` ask for every secret (password, TOTP seed, card number, security
code) with the input hidden, or read them one per line from stdin when it
is not a terminal; a secret given as a flag exits 2. `vault rm <handle>`
asks you to type the handle (or `--yes`). `vault sources [--enable|
--disable onepassword|bitwarden]` shows and toggles the password-manager
login sources (`vault.onepassword.enabled`, `vault.bitwarden.enabled`),
which this build does not read yet.

The vault is `<profile>/vault/vault.enc` (XChaCha20-Poly1305, 0600). Its key
is `<profile>/vault/vault.key` (0600) with `vault.key_storage = "file"`
(default), or an OS keychain entry named in `vault.keyref` with
`"keychain"` (a new vault falls back to the file, with a warning, when no
keychain answers). `prometheus vault key` shows where the key is;
`prometheus vault key migrate --to keychain|file` writes it to the
target, checks that it opens the vault and only then removes the old copy,
so an interrupted move leaves a vault that still opens and running it
again finishes it. A vault whose key is gone fails with `vault key not
found; restore it or delete vault.enc to start over`: it is never re-keyed
behind your back.

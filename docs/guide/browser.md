# Browser automation

[`browser_exec`](../reference/tools.md#browser_exec) (toolset `browser`,
part of `core`) lets the model drive a real browser by running Python 3.
It is offered only in sessions that also have the `terminal` toolset.

```console
$ prometheus tools explain browser_exec
<...>
exposure = exposed
$ prometheus config set browser.provider browserbase
$ prometheus config validate # exits 78
<...>browser.provider: unknown browser provider 'browserbase' (expected local)
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
| `cdp(method, params=None)` | sends a raw DevTools command |
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

1. an explicit DevTools endpoint: `browser.cdp_url`;
2. the cloud browser provider named by `browser.provider` (default
   `local`; provider plugins arrive in a later release, so any other name
   fails `prometheus config validate` with exit 78). When the provider
   fails, a local browser is used and results carry
   `fallback_from_cloud: true`; when both fail the error names both
   reasons;
3. a local headless Chromium: the managed `chromium` runtime, else a
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
profile is never used (this release has no real-profile mode yet; when it
ships it needs both `browser.use_real_profile` and the grant).

## Safety

Before every navigation (and any `url` parameter of `cdp`):

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
`cdp('Page.handleJavaScriptDialog', {...})`) and dismisses them after
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

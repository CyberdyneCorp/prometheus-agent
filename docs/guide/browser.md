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
   Chrome, Chromium, Brave or Edge installed on the host, else the runtime
   installed on first use when lazy installs are allowed
   (`runtimes.allow_lazy_install`, part of the managed runtime store);
   otherwise the call fails with `chromium is not installed and lazy
   installs are disabled: run 'prometheus runtimes install chromium'`.

Each local browser gets a throwaway profile under
`<profile>/cache/browser/profiles/` and is recorded under
`<profile>/cache/browser/records/`, so a later process reaps the browser
of an owner that crashed (after checking its pid, start time and profile
directory). A browser that died is replaced on the next call. Sessions
idle longer than `browser.inactivity_timeout_secs` (default 120, read when
checked) are closed.

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

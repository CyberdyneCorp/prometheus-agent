# Web Dashboard

## Purpose
The web dashboard is a browser SPA embedded in the `prometheus` binary and served by the gateway daemon's HTTP listener, loopback-only by default. It manages sessions, chat, configuration, skills, plugins, schedules and adapters, and adds cost/usage and harness-evolution views. The key decisions are that chat is rendered from the structured client protocol (the same components the desktop uses, no PTY bridge), that every profile-data route names its profile explicitly, and that any non-loopback bind fails closed unless real authentication and TLS are configured.

## ADDED Requirements

### Requirement: Dashboard command and listener defaults
`prometheus dashboard` SHALL ensure the gateway daemon is running, enable its dashboard listener and open the browser with a one-time login link unless `--no-open` is given; the listener SHALL bind `dashboard.host` (default `127.0.0.1`) and `dashboard.port` (default `9120`, `0` = OS-assigned), and `--host`/`--port` SHALL override them for the daemon's lifetime only; `prometheus dashboard --status` SHALL print the URL and exit 0, or exit 69 when no listener is running.
**Milestone:** M3

#### Scenario: Default launch
- **WHEN** the user runs `prometheus dashboard`
- **THEN** the listener SHALL bind `127.0.0.1:9120` and the browser SHALL open a URL carrying a one-time login ticket

#### Scenario: Port conflict
- **GIVEN** another process holds port 9120
- **WHEN** the dashboard listener starts
- **THEN** the command SHALL exit 78 with `dashboard port 9120 is in use; set dashboard.port or pass --port`

### Requirement: SPA is embedded in the binary
The SPA assets SHALL be compiled into the binary with content hashes, so a missing or partial frontend build cannot occur at runtime; `index.html` SHALL be served with `Cache-Control: no-store`, hashed assets with `Cache-Control: public, max-age=31536000, immutable`, a miss under `/assets/` SHALL return 404 (never the SPA shell), and any other non-API path SHALL return the SPA shell.
**Milestone:** M3

#### Scenario: Unknown asset
- **WHEN** the browser requests `/assets/missing-abc123.js`
- **THEN** the response SHALL be 404 with no HTML body

#### Scenario: Deep link reload
- **WHEN** the browser reloads `/sessions/42`
- **THEN** the response SHALL be the SPA shell with status 200

### Requirement: Loopback authentication with an HttpOnly session cookie
On a loopback bind, the browser SHALL authenticate by opening a one-time login URL `/login?ticket=<t>` (ticket minted over the daemon's owner-only control socket, single use, 60 s TTL), which the server SHALL exchange for a `prometheus_session` cookie with `HttpOnly`, `SameSite=Strict`, `Path=/` and a 12 h idle expiry, then redirect to `/`; no credential SHALL ever be injected into served HTML or JavaScript, and an `/api/*` request without a valid cookie SHALL receive `401 {"error": "unauthorized", "login": "run 'prometheus dashboard' to get a login link"}`.
**Milestone:** M3

#### Scenario: Reused login link
- **WHEN** a login URL is opened a second time
- **THEN** the server SHALL respond 401 and SHALL NOT set a cookie

#### Scenario: Page source has no token
- **WHEN** a client fetches `/`
- **THEN** the body SHALL contain no session token or ticket

### Requirement: CSRF protection on state-changing requests
Every non-GET `/api/*` request and every WebSocket upgrade SHALL require an `X-Prometheus-CSRF` header (or `csrf` subprotocol entry for WebSockets) equal to the CSRF token bound to the cookie session, else respond `403 {"error": "csrf"}` (close code 4403 for WebSockets).
**Milestone:** M3

#### Scenario: Cross-site form post
- **WHEN** a third-party page submits a form POST to `/api/profiles/default/sessions/1/delete` with the user's cookie
- **THEN** the cookie SHALL not be sent because of `SameSite=Strict`, and a same-site request without the header SHALL receive 403

### Requirement: Non-loopback binds fail closed
When `dashboard.host` is not a loopback address (RFC 1918, CGNAT and link-local count as non-loopback), startup SHALL require `dashboard.auth.mode` to be `password` (argon2id hash kept in the secrets store), `oidc` (issuer, client id and allowed subjects configured), `portal` or `token` (see Additional dashboard sign-in providers), and SHALL require TLS via `dashboard.tls.cert`/`dashboard.tls.key` or `dashboard.trust_proxy_tls = true`; otherwise the command SHALL exit 78 with `Refusing to bind dashboard to <host>: <missing item>. There is no unauthenticated remote dashboard.`
**Milestone:** M3

#### Scenario: LAN bind without auth
- **WHEN** the user runs `prometheus dashboard --host 0.0.0.0` with no `dashboard.auth.mode`
- **THEN** the command SHALL exit 78 naming `dashboard.auth.mode` and SHALL NOT listen

#### Scenario: Password mode login
- **GIVEN** `dashboard.auth.mode = "password"` and TLS configured
- **WHEN** a browser posts a wrong password 5 times within 60 s from one address
- **THEN** further attempts from that address SHALL get 429 for 300 s

### Requirement: Host header validation
Every HTTP request and WebSocket upgrade SHALL be rejected (400 `Invalid Host header`, close 4403 for WebSockets) unless its Host matches the bound loopback names (`localhost`, `127.0.0.1`, `[::1]` with the bound port), the exact bound host, or `dashboard.public_host`; a malformed Host SHALL be rejected.
**Milestone:** M3

#### Scenario: DNS rebinding
- **WHEN** a request arrives with `Host: evil.example:9120` on a loopback bind
- **THEN** the response SHALL be 400

### Requirement: Same-origin only
The dashboard SHALL send no CORS grant to any origin, SHALL reject WebSocket upgrades whose `Origin` differs from the dashboard's own origin with close code 4403, and SHALL set `Content-Security-Policy: default-src 'self'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer` on every response.
**Milestone:** M3

#### Scenario: Cross-origin WebSocket
- **WHEN** a page on `http://localhost:3000` opens a WebSocket to the dashboard
- **THEN** the upgrade SHALL close with 4403

### Requirement: Public endpoints
Exactly `GET /api/health` and `GET /login` SHALL be reachable without a session; `/api/health` SHALL return `{"ok": true, "version", "build_id", "protocol_version", "auth_required"}` without reading profile data.
**Milestone:** M3

#### Scenario: Health probe
- **WHEN** an unauthenticated client calls `/api/health`
- **THEN** the response SHALL be 200 with the build commit and SHALL NOT include profile names

### Requirement: Profile-addressed routes
Every route that reads or writes profile data SHALL live under `/api/profiles/{profile}/...` and run under that profile's ProfileScope; an unknown profile SHALL return `404 {"error": "unknown profile <name>"}`, and no route SHALL fall back to a default profile.
**Milestone:** M3

#### Scenario: Deleting a session
- **WHEN** the client calls `DELETE /api/profiles/work/sessions/abc`
- **THEN** only the `work` profile's `state.db` SHALL be touched

#### Scenario: Typo in profile
- **WHEN** the client calls `/api/profiles/wrok/config`
- **THEN** the response SHALL be 404 naming `wrok`

### Requirement: REST endpoint inventory
The dashboard backend SHALL expose at least these JSON endpoints, every profile-scoped one under `/api/profiles/{profile}/` and all of them generated into the checked-in OpenAPI document `schema/dashboard-api.json` from the same Rust types as the client protocol: machine-wide `GET /api/health`, `GET /api/status` (daemon version, uptime, clients, adapters), `GET /api/profiles`, `GET /api/logs`, `GET /api/events`, `/api/system/platform`, `/api/kanban/...` and `/api/plugins/...`; per profile `GET|PATCH config`, `GET config/schema`, `GET config/defaults`, `GET|PUT|DELETE secrets/{name}` (write-only values), `GET sessions` (paged, `q` full-text search), `GET sessions/{id}/messages`, `DELETE sessions/{id}`, `GET usage` and `usage.csv`, `GET|POST|PATCH|DELETE cron[/{id}]` plus `POST cron/{id}/run`, `GET skills` and `PATCH skills/{name}` (enable or disable), `GET toolsets` and `PATCH toolsets/{name}`, `GET|PATCH memory`, `GET adapters` and `POST adapters/{name}/pause|resume`, `GET|POST pairing[/{code}]`, `GET|POST|DELETE webhooks[/{name}]`, and `POST maintenance/{action}` for bulk maintenance (`prune-sessions`, `vacuum`, `rebuild-fts`, `prune-snapshots`) which SHALL be refused (403) for observer clients and SHALL require `"confirm": true` in the request body; an endpoint not in the schema SHALL answer 404.
**Milestone:** M3

#### Scenario: Config schema served from the typed config
- **WHEN** an authenticated client calls `GET /api/profiles/work/config/schema`
- **THEN** the response SHALL be the JSON Schema exported from the typed config of the running build

#### Scenario: Bulk maintenance needs confirmation
- **WHEN** a client calls `POST /api/profiles/work/maintenance/prune-sessions` without `"confirm": true`
- **THEN** the response SHALL be 409 and nothing SHALL be deleted

### Requirement: Reverse-proxy base path from config
The dashboard SHALL serve under `dashboard.base_path` (default `/`), rewriting asset URLs and the SPA router base from that value; `X-Forwarded-*` headers SHALL be honoured only when `dashboard.trust_proxy = true` and the peer is in `dashboard.trusted_proxies`.
**Milestone:** M3

#### Scenario: Behind a proxy at /prometheus
- **GIVEN** `dashboard.base_path = "/prometheus"`
- **WHEN** the browser loads `/prometheus/`
- **THEN** every asset URL in the page SHALL start with `/prometheus/assets/`

### Requirement: Structured chat over the client protocol
The dashboard chat SHALL connect to `/api/rpc` (WebSocket, the client protocol with `client_kind: "dashboard"`) and render transcripts, streaming deltas, tool calls, approvals, clarify prompts and subagent status with the same components as the desktop app; the dashboard SHALL NOT spawn a PTY, shell or TUI process for chat.
**Milestone:** M3

#### Scenario: Approval from the browser
- **WHEN** the agent requests approval in a session open in the dashboard
- **THEN** the browser SHALL render the approval card and answer with `approval.respond` over `/api/rpc`

### Requirement: Remote dashboard users are observers by default
Clients authenticated through `password`, `oidc`, `portal` or `token` mode SHALL attach to sessions as `observer` unless their subject is listed in `dashboard.auth.drivers`; loopback cookie sessions SHALL attach as `driver`.
**Milestone:** M3

#### Scenario: OIDC viewer submits
- **GIVEN** an OIDC user not listed in `dashboard.auth.drivers`
- **WHEN** the user tries to send a message
- **THEN** the composer SHALL be disabled and a forced `prompt.submit` SHALL return `-32002`

### Requirement: Live event stream
`/api/events` (WebSocket) SHALL stream daemon-level events the user may see (session started/ended, turn finished, approval pending, cron run, adapter status, budget threshold reached) filtered to the requested profiles, each with `seq`; a reconnect with `since=<seq>` SHALL replay retained events or signal `truncated: true`.
**Milestone:** M3

#### Scenario: Approval badge
- **GIVEN** the dashboard open on the sessions page
- **WHEN** a session in profile `work` starts waiting for approval
- **THEN** an `approval.request` event SHALL arrive and the session row SHALL show a pending badge

### Requirement: Built-in pages
The SPA SHALL provide the routes `/chat`, `/sessions`, `/usage`, `/models`, `/skills`, `/plugins`, `/mcp`, `/cron`, `/adapters`, `/pairing`, `/webhooks`, `/memory`, `/logs`, `/status`, `/profiles`, `/config`, `/secrets`, `/kanban`, `/evolution`, `/experience`, `/files`, `/docs`, `/profile-builder`, `/themes` and `/achievements`, plus `/app` when the webapp is enabled, plus routes contributed by enabled plugins; an unknown route SHALL render a not-found page with a link to `/sessions`.
**Milestone:** M3

#### Scenario: Status page exists
- **WHEN** the user opens `/status`
- **THEN** the page SHALL show daemon version, uptime, attached clients, running sessions, adapter states and the platform support matrix

### Requirement: Secrets are write-only
The `/secrets` page and its API SHALL list secret names, source (secrets store, OS keychain, environment) and last-modified time, and allow set and delete, but SHALL never return a secret value; there SHALL be no reveal endpoint.
**Milestone:** M3

#### Scenario: Reading a key
- **WHEN** a client calls `GET /api/profiles/default/secrets`
- **THEN** the response SHALL contain names and metadata only, with no value or value prefix

### Requirement: Config editing is schema-driven
The `/config` page SHALL render forms from the config JSON Schema of the running build, save through the same atomic validated `config.patch` the desktop uses, and mark settings that apply only from the next session with `applies next session`.
**Milestone:** M3

#### Scenario: Invalid value
- **WHEN** the user sets `busy.input_mode` to `"maybe"`
- **THEN** the save SHALL be refused with the validator's allowed values and the file SHALL be unchanged

### Requirement: Honest platform support matrix
The daemon SHALL compute a platform support matrix (feature name, `supported`, `degraded` or `unsupported`, and a reason) from runtime probes on the current OS, expose it at `GET /api/system/platform` and as `platform_support` in the `rpc.hello` result, and every feature refusal SHALL cite that reason; error messages SHALL never attribute a missing dependency to the operating system or vice versa.
**Milestone:** M3

#### Scenario: Sandbox strength on Windows
- **WHEN** the user opens `/status` on Windows
- **THEN** the matrix SHALL show the local sandbox row with the enforcement actually available and the reason for any `degraded` state

### Requirement: Cost and usage view (upgrade)
The `/usage` page SHALL show cost and tokens from the usage ledger, attributed to root session lineage (children and continuations included), with filters for profile, model, provider, source surface and date range, daily and per-session breakdowns, cache-hit ratio, and current spend against configured budgets; it SHALL offer CSV export of the filtered rows via `GET /api/profiles/{profile}/usage.csv`.
**Milestone:** M3

#### Scenario: Delegation cost rolls up
- **GIVEN** a session whose subagents spent 0.40 and whose own turns spent 0.10
- **WHEN** the user opens the session in `/usage`
- **THEN** the session total SHALL be 0.50 with the children listed beneath it

#### Scenario: Budget warning
- **GIVEN** a monthly budget of 20.00 and spend of 18.50
- **WHEN** the user opens `/usage`
- **THEN** the budget bar SHALL show 92.5 percent used

### Requirement: Harness-evolution view (upgrade)
The `/evolution` page SHALL list harness-evolution runs per profile with mode, budget, rollouts and improver sessions used, cost and status; a run's detail SHALL show each Propose-Probe-Compose round, the candidate harness diffs against the baseline, and the blind selector's pick; the user SHALL be able to adopt or discard the selected harness, and adoption SHALL state that it applies from the next session.
**Milestone:** M3

#### Scenario: Adopting a harness
- **WHEN** the user clicks Adopt on a finished run
- **THEN** the harness SHALL be adopted for new sessions only and every live session's system prompt SHALL be unchanged

### Requirement: Experience library view (upgrade)
The `/experience` page SHALL list domains with their stored lessons and adopted harnesses, show the tasks each lesson came from, and allow the user to disable or delete a lesson; a disabled lesson SHALL not be offered to new sessions.
**Milestone:** M3

#### Scenario: Removing a bad lesson
- **WHEN** the user deletes a lesson from domain `rust-refactors`
- **THEN** sessions started afterwards in that domain SHALL not receive it

### Requirement: Logs page is redacted and bounded
The `/logs` page SHALL tail the daemon and per-profile logs through `GET /api/logs?file=<name>&lines=<n>` with `n` at most 5000, accept only file names from the log registry (never a path), and apply secret redaction before sending.
**Milestone:** M3

#### Scenario: Path traversal
- **WHEN** a client requests `/api/logs?file=../../secrets.toml`
- **THEN** the response SHALL be 400 `unknown log file`

### Requirement: Upload and download limits
File upload for chat attachments SHALL be capped at `dashboard.max_upload_bytes` (default 25 MiB) before the body is buffered, and downloads SHALL serve only blobs referenced by a session of the addressed profile.
**Milestone:** M3

#### Scenario: Oversized upload
- **WHEN** a client uploads a 40 MiB file
- **THEN** the response SHALL be 413 and no bytes SHALL be written to the blob store

### Requirement: Listener lifecycle within the daemon
The dashboard listener SHALL run inside the gateway daemon (one per Prometheus home), SHALL be stoppable with `prometheus dashboard --stop` without stopping the daemon, and SHALL shut down within 3 s on daemon retirement, closing sockets with 1001.
**Milestone:** M3

#### Scenario: Second dashboard command
- **GIVEN** a running dashboard listener
- **WHEN** the user runs `prometheus dashboard` again
- **THEN** no second listener SHALL start and the browser SHALL open a new login link to the existing one

### Requirement: Additional dashboard sign-in providers
`dashboard.auth.mode` SHALL also accept `portal` (sign-in through the hosted account portal's OAuth, verifying that the account owns the dashboard's registered client `dashboard.auth.portal.client_id`) and `token` (a single deployment credential read from the secrets store entry `dashboard_session_token`, compared in constant time on both `Authorization: Bearer` requests and the WebSocket upgrade, with no browser sign-in form); `GET /api/health` SHALL report the active provider names in `auth_providers`, and an OAuth callback URL SHALL be `<dashboard.public_url>/auth/callback` when `dashboard.public_url` is set, else derived from trusted proxy headers.
**Milestone:** M3

#### Scenario: Token deployment
- **GIVEN** `dashboard.auth.mode = "token"` and the secret `dashboard_session_token` set
- **WHEN** a client calls `/api/profiles/default/sessions` with the matching bearer token over TLS
- **THEN** the response SHALL be 200

#### Scenario: Token mode without the secret
- **GIVEN** `dashboard.auth.mode = "token"` and no `dashboard_session_token` secret
- **WHEN** the dashboard starts on a non-loopback host
- **THEN** the command SHALL exit 78 naming `dashboard_session_token`

### Requirement: Registering a dashboard with the account portal
`prometheus dashboard register [--name <label>] [--redirect-uri <https url>]` SHALL use the profile's existing portal sign-in to register a self-hosted OAuth client for this dashboard, write the returned client id to `dashboard.auth.portal.client_id` through a validated config patch, print the registered name, and exit 78 with `sign in to the portal first: prometheus auth login portal` when no portal login exists; a `--redirect-uri` that is not `https` (other than loopback) SHALL be refused with exit 2.
**Milestone:** M3

#### Scenario: Register with a public callback
- **WHEN** the user runs `prometheus dashboard register --redirect-uri https://dash.example.com/auth/callback`
- **THEN** the client id SHALL be saved to `dashboard.auth.portal.client_id` and the command SHALL exit 0

### Requirement: Paired device credentials for remote clients
`prometheus dashboard pair [--name <device>] [--role driver|observer]` SHALL print a one-use pairing code valid for 10 minutes that a desktop app exchanges over TLS for a per-device credential bound to that device name and role (default `driver`); device credentials SHALL be stored server-side only as hashes in the daemon's credential store, rotate on each refresh, be listed by `prometheus dashboard devices` and revoked by `prometheus dashboard devices revoke <name>`, which SHALL close that device's sockets with 4401 within 1 s; pairing SHALL work in every `dashboard.auth.mode` and SHALL itself still require TLS on non-loopback binds.
**Milestone:** M3

#### Scenario: Revoking a lost laptop
- **GIVEN** a paired device `old-laptop` with an open session
- **WHEN** the user runs `prometheus dashboard devices revoke old-laptop`
- **THEN** its sockets SHALL close with 4401 and its credential SHALL be rejected afterwards

#### Scenario: Expired pairing code
- **WHEN** a pairing code is presented 11 minutes after it was printed
- **THEN** the exchange SHALL fail with 401 and no credential SHALL be issued

### Requirement: Remote desktop clients attach through the dashboard listener
Remote desktop apps and webapp tabs SHALL use the dashboard listener's `/api/rpc` endpoint under the same Host, Origin, TLS and authentication rules as browsers, so serving remote clients needs no second listener; `prometheus serve [--host <h>] [--port <p>]` SHALL run the gateway daemon in the foreground with the dashboard listener enabled and no browser (for service managers), applying the fail-closed non-loopback rules, and `prometheus dashboard --fingerprint` SHALL print the SHA-256 public-key fingerprint of the configured TLS certificate for pinning.
**Milestone:** M3

#### Scenario: Headless VPS
- **GIVEN** TLS and `dashboard.auth.mode = "portal"` configured
- **WHEN** a service manager runs `prometheus serve --host 0.0.0.0`
- **THEN** the daemon SHALL serve native clients and the dashboard on that listener until stopped

### Requirement: Desktop workspace in the browser
When `webapp.enabled = true` (default false) the dashboard listener SHALL serve the desktop workspace (sessions, chat, profiles, capabilities, messaging, artifacts, scheduled jobs, file tree, Git review, previews and terminals, rendered by the desktop's components) under `/app`; `prometheus webapp [--no-open] [--status] [--sign-out]` SHALL ensure the daemon and open a one-use sign-in link (60 s, single use; printed instead of opened with `--no-open` or when no browser can be launched), `--status` SHALL print whether and where the webapp is served or why not, and `webapp.public_url` (default empty) SHALL be the address opened and accepted as Host when set; native-only features (global shortcuts, always-on-top windows, window reading, the updater) SHALL be absent in the webapp, and `desktop_ui` tools that need them SHALL report so.
**Milestone:** M3

#### Scenario: Opening from the CLI
- **GIVEN** `webapp.enabled = true`
- **WHEN** the user runs `prometheus webapp`
- **THEN** the browser SHALL open `/app` signed in through a one-use link and a second use of the link SHALL get 401

#### Scenario: Webapp disabled
- **GIVEN** `webapp.enabled = false`
- **WHEN** the user runs `prometheus webapp --status`
- **THEN** the command SHALL print `webapp disabled (set webapp.enabled = true)` and exit 69

### Requirement: Webapp sign-out and insecure-context notice
`prometheus webapp --sign-out` SHALL invalidate every browser sign-in to this listener (cookie sessions and their CSRF tokens), closing their sockets, PTY attachments and screen viewers with 4401, while paired native desktop devices stay signed in; `prometheus webapp` and `--status` SHALL warn when the address a browser would open is plain HTTP on a non-loopback host that microphone, clipboard reading, notifications and screen viewing are unavailable there.
**Milestone:** M3

#### Scenario: Sign out every tab
- **GIVEN** three webapp tabs signed in on two browsers
- **WHEN** the user runs `prometheus webapp --sign-out`
- **THEN** all three tabs SHALL show the sign-in instructions and their next API calls SHALL get 401

### Requirement: Webapp file staging and terminals
Files a webapp user selects, pastes or drops SHALL be staged under the addressed profile before entering the attachment flow, capped at 16 MiB per file, host files SHALL download only through the session's file-access rules, URL previews SHALL load in a sandboxed frame that cannot open popups, and webapp terminals SHALL be the daemon-owned PTYs of the desktop (one attachment per PTY; a second tab attaching takes it over and the first is detached).
**Milestone:** M3

#### Scenario: Oversized drop
- **WHEN** a user drops a 20 MiB file onto the webapp terminal
- **THEN** the upload SHALL be refused with 413 before any bytes are written

### Requirement: Kanban dashboard API and orchestration modes
The `/kanban` page SHALL show the shared boards with an Orchestration Auto/Manual control bound to `kanban.auto_decompose` and an orchestration settings panel (`GET`/`PUT /api/kanban/orchestration` returning configured and effective values and rejecting unknown profile names with 400), and the daemon SHALL expose `GET /api/kanban/workers/active` (worker id, profile, task id, started at, last heartbeat), `GET /api/kanban/runs/{run_id}` (task id, status, start, end, exit code, log name), `POST /api/kanban/runs/{run_id}/terminate` (stop a reclaimable run and free its task for dispatch) and `GET /api/kanban/inspect` (backlog, in-progress versus the in-progress limit, recent events), all behind the dashboard's authentication and CSRF rules.
**Milestone:** M3

#### Scenario: Terminate a stuck run
- **GIVEN** a running worker on task `t_12`
- **WHEN** the user clicks Terminate on its run
- **THEN** the worker SHALL be stopped and `t_12` SHALL return to `ready`

#### Scenario: Unknown orchestrator profile
- **WHEN** a client sends `PUT /api/kanban/orchestration {"orchestrator_profile": "ghost"}`
- **THEN** the response SHALL be 400 naming `ghost` and configuration SHALL be unchanged

### Requirement: Webhooks page
The `/webhooks` page SHALL list the profile's webhook subscriptions with URL, event filter, delivery target and enabled state, create a subscription (name, description, event filter, delivery target, direct-delivery flag, agent prompt) showing its route URL and HMAC secret exactly once, enable or disable it without a restart (a disabled route answers 403), delete it, and send a signed test event; when the webhook adapter is disabled the page SHALL say so with a link to `/adapters`.
**Milestone:** M3

#### Scenario: Secret shown once
- **GIVEN** a subscription just created
- **WHEN** the user reloads the page
- **THEN** the HMAC secret SHALL no longer be displayed and no API SHALL return it

### Requirement: Channel setup forms
The `/adapters` page SHALL list every adapter available to the profile with its messaging state (the states of the desktop Messaging pane), render each adapter's configure form from its options schema with secret fields write-only (blank keeps the stored value), offer enable/disable, Test and Restart adapter actions, and host interactive pairing flows declared by an adapter (WhatsApp QR pairing streamed as refreshed QR images until linked or 120 s, Telegram Create with QR); a form save SHALL be one validated config patch plus secret writes, applied atomically.
**Milestone:** M3

#### Scenario: Required field missing
- **WHEN** the user saves the Matrix form without a homeserver URL
- **THEN** the save SHALL be refused naming the field and nothing SHALL be written

### Requirement: Files page
The `/files` page SHALL browse a profile's workspace roots (the profile's configured workspace and session workspaces on the local backend) with list, preview (text up to 2 MiB, images), download, upload (capped by `dashboard.max_upload_bytes`), rename and delete after confirmation, resolving every path server-side and refusing paths that escape the chosen root (including through symlinks) with 403.
**Milestone:** M3

#### Scenario: Symlink escape
- **GIVEN** a workspace symlink pointing at `/etc`
- **WHEN** the user opens it in `/files`
- **THEN** the response SHALL be 403 and no entry outside the root SHALL be listed

### Requirement: Embedded documentation
The `/docs` page SHALL serve the user documentation of the running build, embedded in the binary and searchable offline, without fetching remote content, and every `applies next session` or error message in the dashboard that names a config key SHALL link to that key's generated reference entry.
**Milestone:** M3

#### Scenario: Offline docs
- **GIVEN** a machine without internet access
- **WHEN** the user opens `/docs` and searches `busy.input_mode`
- **THEN** the reference entry for the key SHALL be shown

### Requirement: Profile builder
The `/profile-builder` page SHALL create a profile through guided steps (name and description, model and provider, persona `SOUL.md`, toolsets, skills, adapters) and, on finish, call the same `profile.create` operation as `prometheus profile create` followed by one validated config patch, creating nothing when any step fails validation; it SHALL offer cloning an existing profile's configuration without its secrets, sessions or memories.
**Milestone:** M3

#### Scenario: Clone without secrets
- **WHEN** the user builds profile `writer` by cloning `default`
- **THEN** `writer` SHALL have `default`'s configuration and no secrets, sessions or memories

### Requirement: Dashboard themes
The dashboard and webapp SHALL support themes defined as TOML files with a palette (`background`, `midground`, `foreground` colours with alpha, `warm_glow`, `noise_opacity` 0–1.2), typography (font families from locally installed or embedded fonts only, base size, line height), layout (`radius`, `density` in `compact|comfortable|spacious`), `layout_variant` in `standard|cockpit|tiled`, component colour overrides and `custom_css` of at most 32 KiB; built-in themes SHALL be `default`, `default-large`, `midnight`, `ember`, `mono`, `cyberpunk` and `rose`, user themes SHALL be discovered from the machine theme directory with validation errors shown in `/themes`, and CSS containing `@import` or `url()` to another origin SHALL be rejected.
**Milestone:** M3

#### Scenario: Theme imports remote CSS
- **WHEN** a user theme's `custom_css` contains `@import url(https://x.example/a.css)`
- **THEN** the theme SHALL be rejected in `/themes` with the reason and the current theme SHALL stay applied

#### Scenario: Oversized custom CSS
- **WHEN** a theme declares 40 KiB of `custom_css`
- **THEN** the theme SHALL be rejected naming the 32 KiB limit

### Requirement: Dashboard plugin contributions
Plugins that declare a `dashboard` entry in `plugin.toml` (label, icon, `tab.path`, `tab.position` as `end|after:<page>|before:<page>`, optional `tab.override` of one built-in route, `tab.hidden`, slots) SHALL render in sandboxed frames with an opaque origin and talk to the dashboard only through the plugin UI SDK message channel with their granted capabilities; slots SHALL be the shell slots `header-left`, `header-right`, `header-banner`, `sidebar` (cockpit variant only), `pre-main`, `post-main`, `footer-left`, `footer-right`, `overlay` and `<page>:top|bottom` for each built-in page; when two plugins override the same route the first in load order SHALL win and the other SHALL be reported on `/plugins`; plugin backend routes SHALL be served at `/api/plugins/<id>/` by the plugin host under the plugin's grants, never in the daemon process; `POST /api/plugins/rescan` SHALL reload discovery, and a plugin that has not registered within 2 s SHALL not block the first render.
**Milestone:** M3

#### Scenario: Override conflict
- **GIVEN** plugins `a` and `b` both declaring `tab.override = "/sessions"`
- **WHEN** the dashboard loads
- **THEN** `a`'s page SHALL render at `/sessions` and `/plugins` SHALL show that `b`'s override was ignored

#### Scenario: Plugin frame reads the session cookie
- **WHEN** a plugin frame reads `document.cookie`
- **THEN** it SHALL not see the `prometheus_session` cookie

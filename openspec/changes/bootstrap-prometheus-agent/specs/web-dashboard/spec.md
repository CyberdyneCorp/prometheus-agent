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
When `dashboard.host` is not a loopback address (RFC 1918, CGNAT and link-local count as non-loopback), startup SHALL require `dashboard.auth.mode` to be `password` (argon2id hash kept in the secrets store) or `oidc` (issuer, client id and allowed subjects configured), and SHALL require TLS via `dashboard.tls.cert`/`dashboard.tls.key` or `dashboard.trust_proxy_tls = true`; otherwise the command SHALL exit 78 with `Refusing to bind dashboard to <host>: <missing item>. There is no unauthenticated remote dashboard.`
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
Clients authenticated through `password` or `oidc` mode SHALL attach to sessions as `observer` unless their subject is listed in `dashboard.auth.drivers`; loopback cookie sessions SHALL attach as `driver`.
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
The SPA SHALL provide the routes `/chat`, `/sessions`, `/usage`, `/models`, `/skills`, `/plugins`, `/mcp`, `/cron`, `/adapters`, `/pairing`, `/webhooks`, `/memory`, `/logs`, `/status`, `/profiles`, `/config`, `/secrets`, `/kanban`, `/evolution` and `/experience`, plus routes contributed by enabled plugins; an unknown route SHALL render a not-found page with a link to `/sessions`.
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

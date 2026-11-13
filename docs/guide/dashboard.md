# The web dashboard

The web dashboard is a browser app built into the `prometheus` binary and
served by the gateway daemon. It shows your sessions, lets you chat with
the agent and answer its approvals, and covers usage and cost, the
configuration, secrets, logs and the daemon's status. Chat uses the same
client protocol as the CLI, the TUI and the desktop app. No terminal runs
behind the page.

## Opening it

```text
$ prometheus dashboard
Dashboard: http://127.0.0.1:9120/
Opened a one-time login link in your browser.
```

`prometheus dashboard` starts the daemon when it is not running, opens
the daemon's dashboard listener and signs your browser in through a
one-time login link. The link is valid once, for 60 seconds. With
`--no-open`, or when no browser can be started, the link is printed
instead. Run the command again to get a new link: the running listener is
reused, and no second listener starts.

| Flag | What it does |
|---|---|
| `--host <addr>` / `--port <n>` | Bind another address or port until the daemon stops (`--port 0` picks a free port). |
| `--no-open` | Print the login link instead of opening a browser. |
| `--status` | Print the dashboard's URL. Exits 69 when no listener runs. |
| `--stop` | Stop the listener. The daemon keeps running. |

Without a running listener, `--status` says so and exits 69:

```console
$ prometheus dashboard --status # exits 69
<...>no dashboard listener is running<...>
```

When the port is taken, the command exits 78 with `dashboard port 9120 is
in use; set dashboard.port or pass --port`.

The listener's settings are machine-wide and live in `<home>/daemon.toml`
(all keys: [Daemon configuration](../reference/daemon-configuration.md)):

```toml
[dashboard]
host = "127.0.0.1"          # loopback by default
port = 9120                 # 0: any free port
base_path = "/"             # a reverse proxy's mount point, e.g. "/prometheus"
max_upload_bytes = 26214400 # chat attachments (25 MiB)
```

## Signing in and security

- The browser holds one cookie, `prometheus_session` (`HttpOnly`,
  `SameSite=Strict`), which expires after 12 hours without use. Served
  pages never contain a credential. Every API call without the cookie gets
  `401` with a hint to run `prometheus dashboard`.
- Every state-changing call needs the session's CSRF token in
  `X-Prometheus-CSRF`, and every WebSocket needs it as a `csrf.<token>`
  subprotocol. Otherwise the call gets `403 {"error": "csrf"}`, or the
  socket closes with 4403.
- Requests are accepted only for the listener's own host names: the
  loopback names with the bound port, the bound host, or
  `dashboard.public_host`. Any other `Host` header gets `400` (protection
  against DNS rebinding). WebSockets from another origin close with 4403.
  The dashboard never sends a CORS header.
- Only `GET /api/health` and the sign-in pages are public.

### Remote access fails closed

A non-loopback address needs real sign-in and TLS. Private (RFC 1918),
CGNAT and link-local addresses are not loopback. Without both, the command
exits 78 and the listener never opens:

```text
$ prometheus dashboard --host 0.0.0.0
Refusing to bind dashboard to 0.0.0.0: dashboard.auth.mode. There is no unauthenticated remote dashboard.
```

Choose a sign-in provider and give the dashboard TLS, either its own
certificate or a trusted proxy that terminates TLS:

```toml
[dashboard]
host = "0.0.0.0"
public_host = "dash.example.com"

[dashboard.tls]
cert = "/etc/prometheus/dash.pem"
key = "/etc/prometheus/dash-key.pem"
# or: trust_proxy_tls = true, with trust_proxy = true and trusted_proxies = ["10.0.0.2"]

[dashboard.auth]
mode = "password"            # or "oidc"
drivers = ["password"]       # subjects that may send messages
```

- `password` checks the password against the argon2id PHC hash stored in
  the launch profile's secret `DASHBOARD_PASSWORD_HASH`. To set it, create
  the hash with any argon2id tool, for example `argon2 "$(openssl rand
  -hex 8)" -id -e`, and pipe it to `prometheus secrets set
  DASHBOARD_PASSWORD_HASH`. After 5 wrong passwords within a minute, an
  address gets `429` for 5 minutes.
- `oidc` signs users in through `[dashboard.auth.oidc]` (`issuer`,
  `client_id`, `allowed_subjects`). It uses the authorization code flow
  with PKCE, checks the ID token's signature (ES256 or RS256) and the
  claims, and uses `<origin>/auth/callback` as the redirect URI.

Remote users are **observers**, unless their subject is in
`dashboard.auth.drivers`. Observers have read-only access: they can follow
sessions and read every page, but they cannot send messages, answer
approvals or upload files. They also cannot change anything else, such as
configuration, secrets, sessions or the vault. A change over the API gets
`403 {"error": "observer"}`, and a change over `/api/rpc` gets `-32002`
(`session.observer`). Your own login links always give a driver session.

## Pages

| Page | What it shows |
|---|---|
| `/chat` | A session, rendered from the client protocol: streamed answers, tool calls, subagent status, and approval and clarify cards. |
| `/sessions` | The profile's sessions, with full-text search, deletion and a badge on sessions waiting for an approval. |
| `/usage` | Cost and tokens from the usage ledger, by root session (subagents and continuations are listed under their root), by day, with the cache-hit ratio, the daily and monthly budget bars, and a CSV export. |
| `/status` | The daemon's version, uptime, clients, sessions, adapters and platform support matrix. |
| `/config` | Forms built from the configuration schema of the running build. A save is one validated write. Keys that only apply to new sessions are marked "applies next session". |
| `/secrets` | Secret names, where each one comes from, and when it changed. You can set or delete a secret, but values are write-only: no page or API returns one. |
| `/logs` | The last records (at most 5000) of a log from the daemon's list, redacted. |
| `/models`, `/skills`, `/profiles`, `/adapters` | The profile's catalogs and the daemon's profiles and adapters. |
| `/plugins`, `/mcp`, `/cron`, `/memory` | The profile's plugins (with their load state and any problem), MCP servers, cron jobs (with their state and next run) and memory entries. These pages are read-only for now: change them with `prometheus plugins`, `mcp`, `cron` and `memory`. |

These pages are listed in the navigation, but this build does not
implement them yet, so each one says so: `/pairing`, `/webhooks`,
`/kanban`, `/files`, `/docs`, `/profile-builder`, `/themes` and
`/achievements`. The `/evolution` and
`/experience` pages read the harness-evolution and experience-library
methods of the client protocol, and they report when the daemon does not
serve those yet.

The profile selector in the header picks the profile that each page
addresses. Every API call names its profile in its path
(`/api/profiles/<profile>/...`). An unknown profile gets `404`, and no
call falls back to a default profile.

## The platform support matrix

When it starts, the daemon probes what this host supports and serves the
results at `/status`, at `GET /api/system/platform` and as
`platform_support` in the client protocol's `rpc.hello` result. Each row
names a feature (`sandbox.local`, `containers`, `terminal.pty`,
`secrets.keychain`, `service.autostart`), a state (`supported`, `degraded`
or `unsupported`) and the reason. The reason names whichever is actually
missing: the operating system (no Landlock in this kernel) or a dependency
(no Docker on the `PATH`). The sandbox row reports the enforcement the
host's sandbox (Landlock and seccomp, Seatbelt, or an `AppContainer` on
Windows) gives the baseline policy, without starting a sandbox.

## API

The SPA uses these JSON endpoints, and so can scripts that hold a
session cookie:

- Machine-wide: `GET /api/health` (public), `GET /api/session` (the
  signed-in user, role and CSRF token), `POST /api/logout`,
  `GET /api/status`, `GET /api/profiles`, `GET /api/system/platform`,
  `GET /api/logs[?file=<name>&lines=<n>]`, and the WebSockets
  `/api/rpc` (the client protocol as a `dashboard` client) and
  `/api/events[?profiles=a,b&since=<seq>]` (session started or ended, turn
  finished, approval pending and settled, cron run, adapter state, budget
  threshold, each with a `seq`; on reconnect, `since` replays what was
  missed or reports `truncated`).
- Per profile, under `/api/profiles/<profile>/`: `GET sessions[?q=]`,
  `GET sessions/<id>/messages`, `DELETE sessions/<id>`, `GET|PATCH config`
  (`{"set": {"<key>": <value>}}`), `GET config/schema`,
  `GET config/defaults`, `GET secrets`, `PUT|DELETE secrets/<NAME>`,
  `GET usage` and `usage.csv`,
  `GET skills|toolsets|models|plugins|mcp|cron|memory`,
  `POST uploads` (the file as the body), and `GET blobs/<sha256>` (only
  blobs that a session of the profile references).

# The messaging gateway

The gateway is the part of the daemon that answers on messaging platforms
(Telegram, Discord, Slack, ...). Each served profile enables its own
platforms in its `config.toml`; one daemon per home runs them all, routes
every chat to a session, and delivers every reply through a durable
outbox, so nothing is lost or sent twice across restarts.

## Setup

Enable a platform and name the secret that holds its credential (the
value itself goes in the secrets store, never in `config.toml`):

```console
$ prometheus config set gateway.platforms.telegram.enabled true
$ prometheus config set gateway.platforms.telegram.token_secret TELEGRAM_BOT_TOKEN
```

Then start the daemon (`prometheus daemon start`) and check the adapters
with `prometheus gateway status`; without a daemon the command says so:

```console
$ prometheus gateway status # exits 69
error: the gateway daemon is not running
```

`prometheus gateway reload` starts adapters enabled since the daemon
started, and serves profiles created since then, without touching running
conversations.

### Starting at login

`prometheus daemon install-service` installs a per-user supervisor unit
that starts the daemon at login without administrator rights: a systemd
`--user` unit `prometheus-daemon.service` (with lingering when the system
allows it) on Linux, the LaunchAgent `ai.prometheus.daemon` on macOS, and
the Scheduled Task `Prometheus Daemon` (`ONLOGON`, `/RL LIMITED`) on
Windows, falling back to a launcher in the Startup folder when Task
Scheduler refuses. The unit restarts the daemon after exit 75 (restart,
update) and 1, never after 0 (stop) or 78 (configuration error).
Re-running the command rewrites the same unit; `--no-start` installs
without starting; `prometheus daemon uninstall-service` removes it.
`--system --run-as-user <user>` installs a boot-time unit instead and
needs root (exit 77 otherwise). On other systems (Android/Termux, the
BSDs) the command exits 69 (`service.unsupported`): run
`prometheus daemon run` under your own supervisor. `prometheus doctor`
reports units left by a Hermes gateway (every profile's
`hermes-gateway[-<profile>].service` or `ai.hermes.gateway[-<suffix>]`,
user and system scope) or an earlier layout, with the exact command that
stops and removes each and installs the Prometheus service in its place.
It never removes them itself.

`prometheus gateway list` shows every running daemon of your OS user,
whatever its home (with `--json`: home, pid, start time, build, profiles,
adapters):

```console
$ prometheus gateway list
no Prometheus daemon is running
```

## Who may talk to the agent

Nobody, by default. A sender is admitted by the first rule that matches:
the relay's upstream verdict, `allow_chats`, `allow_all`, an approved
pairing, `allow_users`, then the adapter's own allowlist. Wildcards in
`allow_users` are refused: admitting everyone is the explicit `allow_all`.

### Pairing

A stranger who sends a direct message gets an 8-character code (valid for
an hour). The owner approves it with `prometheus pairing approve <platform>
<code>`, from an attached client, or with `/pair approve <code>` in an
admin chat; spaces, dashes and case do not matter. Five failed approvals
lock pairing for that platform for an hour. Codes are never logged or
stored in plain text.

```console
$ prometheus --json pairing list
{"schema":"prometheus.pairing.v1","pairings":[]}
```

### Command tiers

With `admins` (direct messages) or `group_admins` (groups) set for a
platform, only those users run every command; others keep `/help`,
`/whoami`, `/stop`, `/new`, `/queue`, `/link` and the platform's
`user_commands`. Admin-tier commands include `/pause`, `/sethome` and
`/policy`.

## Policies

### Rate limits

Each sender has a token bucket: `gateway.rate_limit.messages_per_minute`
(default 20) with a `burst` of 10, `turns_per_hour` (120), and an optional
`daily_cost_usd` measured from the usage ledger since local midnight. A
message over a limit is dropped and the sender hears "Slow down: rate
limit reached, try again in `<n>`s." once per window. Admins are exempt;
every key can be overridden under `gateway.platforms.<name>.rate_limit`.

```console
$ prometheus config set gateway.rate_limit.daily_cost_usd 2.0
```

### Per-chat policies

`[[gateway.chat_policies]]` entries match a platform plus a `chat_id`,
`chat_type` or `user_id` (the most specific wins; two entries with the
same matchers fail validation) and set what sessions of those chats get:

```toml
[[gateway.chat_policies]]
platform = "discord"
chat_id = "555"
model = "openai/gpt-5-mini"
toolsets = ["web"]
skills = ["triage"]
system_prompt_append = "Answer in two sentences."
max_cost_usd_per_turn = 0.25
busy = { input_mode = "queue" }
rate_limit = { messages_per_minute = 10 }
```

`toolsets` and `skills` only narrow what the profile enables: a toolset
outside the profile's (or a disabled skill) fails validation. A policy is
frozen with each session when it is created, so an edit reaches new
sessions only; an admin's `/policy apply --now` starts a new session with
the edited policy at once, and `/policy` shows both.

### Busy input

Input that arrives while a turn runs is queued (default), steers the
running turn or interrupts it, per `busy.input_mode`, overridable per
platform (`gateway.platforms.<name>.busy`) and per chat policy.

## Home channels and one-shot sends

Each platform has at most one home channel: where scheduled output,
platform-only send targets and notices without a chat go. `/sethome` (or
`/set-home`) in a chat makes it (and its thread) the home channel, or set
it in `config.toml`:

```console
$ prometheus config set gateway.platforms.telegram.home_channel.chat_id -100123
$ prometheus gateway send --list
telegram:-100123  home
```

`prometheus gateway send --to <target> [message]` sends one message
without an agent turn. Targets are `platform` (its home channel),
`platform:chat_id`, `platform:chat_id:thread_id` or
`platform:#channel-name`; the body comes from the argument, `--file` or
piped stdin, and a line `MEDIA:<path>` attaches a file (`[[as_document]]`
sends images uncompressed). With the daemon running the message goes
through its outbox and the command waits for the delivery; without it,
platforms that can send statelessly send directly and the others need the
daemon:

```console
$ prometheus gateway send --to telegram:-1001234567890 "RAM 92%" # exits 69
error: daemon required for telegram
```

A script typically runs `echo "RAM 92%" | prometheus gateway send --to
telegram -q`, which prints nothing and exits 0 once sent. Exit codes: 0
sent, 1 delivery failure, 2 usage error, 66 unknown target or no home
channel, 69 daemon required, 78 platform not configured.

The agent has the same ability through the [`send_message`](../reference/tools.md#send_message) tool (toolset
`messaging`, which `core` does not include: name it in
`tools.enabled_toolsets`). It may reach only a
home channel, an allowed chat or a chat the profile already has a session
in; anything else is refused ("target telegram:999 is not an authorized
destination"), and so is any send whose authorization check fails. A sent
message is mirrored into the target chat's session.

## Emergency pause

`prometheus pause` stops new work on every profile of the home: no new
turns from any platform or client, no scheduled job. Turns already running
finish and slash commands keep working; every refused message answers
with the reason, and a one-shot CLI turn exits 69. The pause is a file
(`<home>/run/paused`), so it survives daemon restarts. Admins can do the
same from a chat with `/pause [reason|off|status]`.

```console
$ prometheus pause --reason "billing incident"
paused: billing incident; new turns and scheduled jobs are refused until 'prometheus pause off'
$ prometheus pause status
paused since <time> by cli: billing incident
$ prometheus pause off
pause lifted
```

## Relay connector

A relay connector fronts platforms for hosts that cannot hold platform
credentials or open listeners. `prometheus gateway enroll --token <one-time
token> --connector-url https://relay.example` exchanges the single-use
token for a long-lived credential, stores it only in the secrets store
(`PMT_RELAY_TOKEN` by default) and writes `[gateway.relay]` (URL, gateway
id, optional `--wake-url`) to `daemon.toml`; a used or expired token exits
78 and changes nothing. The daemon then dials the connector over TLS
(`gateway reload` connects a relay enrolled while it runs), trusts its
authorization of senders, and leaves the profiles' direct adapters of the
platforms it fronts unconnected unless
`gateway.relay.allow_direct_platforms` lists them.

The connector speaks contract version 1: JSON text frames on one
WebSocket the daemon opens with `Authorization: Bearer <credential>` and
`X-Prometheus-Gateway-Id`. Its first frame is the descriptor
`{"type":"descriptor","contract_version":1,"platforms":[...],
"max_message_length":N,"supports_edit":bool}` (unknown keys are ignored; a
missing length is 4096). It then sends `inbound` frames (`platform`,
optional `profile`, `message_id`, `sender_id`, `chat_id`, `chat_type`,
`thread_id`, `text`), each answered by an `admission` frame; the daemon
sends `send`, `edit` and `egress` requests with an `id`, answered by
`{"type":"result","id":..,"ok":true,"message_id"?,"allow"?}` or
`{"ok":false,"error":{"kind":"rate_limited|network|server|permanent",
"message":..}}`. Enrollment is `POST <connector-url>/v1/enroll` with
`{"token","gateway_id"?,"wake_url"?}`, answered by `{"gateway_id",
"credential","relay_url"?}` or a 4xx `{"error":"already_used"|"expired"}`.

## Health export

With `observability.gateway_health.enabled = true` and an OTLP/HTTP
`endpoint` (HTTPS, or HTTP to a loopback collector), the daemon exports
`prometheus.gateway.*`, `prometheus.adapter.*`, `prometheus.outbox.*` and
`prometheus.cron.*` gauges and counters every `interval_secs` (60). Their
attributes are limited to profile, platform, state, error code, admission
and lifecycle event: never message text, user ids, chat ids or secrets.
An unreachable collector never slows messages down and is logged at most
once a minute. Off by default: nothing is sent.

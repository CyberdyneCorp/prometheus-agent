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
user and system scope; on Windows the Scheduled Task
`Hermes_Gateway[_<suffix>]` and the Startup-folder launchers
`Hermes_Gateway[_<suffix>].vbs`/`.cmd`) or an earlier layout, with the
exact command (cmd.exe syntax on Windows) that stops and removes each and
installs the Prometheus service in its place. It never removes them
itself.

`prometheus gateway list` shows every running daemon of your OS user,
whatever its home (with `--json`: home, pid, start time, build, profiles,
adapters):

```console
$ prometheus gateway list
no Prometheus daemon is running
```

## Platform adapters

The binary ships the `telegram`, `discord`, `slack`, `email`, `matrix`,
`signal`, `whatsapp` and `webhook` adapters; any other platform name is
accepted only when an installed adapter plugin declares it, and an unknown
`[gateway.platforms.<name>]` table fails validation. Names are lower case.

Each adapter declares a capability descriptor (its message limit and the
unit it counts in, media, buttons, editing, typing, threads...) and the
gateway reads limits and capabilities only from there; `prometheus gateway
platforms` prints every loaded descriptor. A platform table may lower
`max_message_length`, never raise it (Telegram 4096 UTF-16 units, Discord
2000, Slack 39000, Email 50000, Matrix 32000 bytes, Signal 8000, WhatsApp
4096, Webhook 65536 bytes).

```toml
[gateway.platforms.discord]
enabled = true
token_secret = "DISCORD_BOT_TOKEN"   # a secret name, never the token
reply_to_mode = "first"              # off | first | all
typing_indicator = true
human_delay_ms = [400, 1200]         # random pause before each part
batching = { delay_ms = 300, split_delay_ms = 1000 }
options = { require_mention = true } # checked against the adapter's schema
```

Settings an enabled adapter cannot start without fail validation and name
the key (for example `gateway.platforms.email.options.imap_host`). Adapters
connect directly: proxy variables in the daemon's environment and the OS
proxy settings are ignored unless `gateway.trust_env = true`. With it, the
variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) come first
and the OS settings fill what they leave unset (macOS network proxies and
their exceptions, Windows Internet Settings; on Linux the variables are
the system settings). `gateway.platforms.<name>.proxy_url`
(else `gateway.proxy_url`) sends an adapter's HTTP and WebSocket traffic
through an `http://`, `https://` or `socks5://` proxy; a proxy URL with a
password goes in a secret named by `proxy_url_secret`.

Telegram reads its bot token from the secret `TELEGRAM_BOT_TOKEN` and
long-polls the Bot API (`options.poll_timeout_secs`, default 30); updates
that arrive while it is disconnected are fetched once it reconnects. With
`options.mode = "webhook"` Telegram posts updates to the shared listener
instead (`options.webhook_public_url` must reach
`/p/<profile>/telegram/`, and `options.webhook_secret_secret` names the
secret token every post must carry; a post without it gets 401). Topics
of a threaded private chat or a forum group are threads of their own:
`options.dm_topics` / `options.group_topics` declare topics per chat
(`{ name, thread_id?, icon_color?, skill? }`), missing private topics are
created at connect (their ids kept in
`<profile>/platforms/telegram/topics.json`), and a topic's `skill` is
loaded into each new session there; `options.ignore_root_dm = true`
ignores ordinary messages outside a topic. The bot's command menu lists
the gateway commands and skills, cut to `options.command_menu.max_commands`
(60) after placing `options.command_menu.priority` first (`priority_mode`
`prepend`, `append` or `replace`); `@bot <term>` searches every command and
skill. The menu is registered again within a few seconds whenever the
commands change while the daemon runs (a skill installed, enabled or
disabled, a quick command edited). Replies are sent as `MarkdownV2`
(small tables as bullet groups). With `options.rich_messages = true` they
are rich messages instead (`sendRichMessage`): the reply's Markdown as
written, up to 32768 characters in one message, so tables render
natively; a rich message Telegram refuses is sent as `MarkdownV2` in
4096-character parts, and a Bot API without rich messages turns them off
until the adapter restarts.

Discord reads `DISCORD_BOT_TOKEN`, connects to the Discord Gateway and
resumes the session after a dropped connection, so missed messages are
replayed; the bot needs the Message Content privileged intent, and
`options.require_mention = true` answers server messages only when they
mention the bot (`options.free_response_channels` excepted). A mention in
a text channel opens a thread named after the message, where the
conversation goes on without mentions and which takes the session's
title after the first turn (`options.auto_thread`, default on; never in
`options.no_thread_channels` or for replies). A send to a forum channel
opens a post titled by its first line. The gateway's commands are
registered as application commands (100 at most), again whenever they
change, unless `options.slash_commands = false`, which a follower daemon
sharing the application sets. Both adapters send buttons for approvals, typing
indicators, reactions and files, and both read the token from the owning
profile's secrets only.

The other first-party adapters:

- **Slack** uses Socket Mode with the app-level token `SLACK_APP_TOKEN`
  (or `options.app_token_secret`) and the Web API with the bot token
  `SLACK_BOT_TOKEN`; Slack keeps events nobody acknowledged and delivers
  them after a reconnect. Cards are Block Kit buttons, edited when an
  approval is resolved; files use Slack's external upload flow. Each
  gateway command is a native slash command (and `/prometheus <command>`
  runs any of them); its output replaces a private "Running /command…"
  note and is never posted publicly. In threads, where Slack has no
  slash commands, `!stop` (after an optional mention) runs `/stop`.
  `prometheus gateway slack-manifest` prints an app manifest with Socket
  Mode, the scopes, the events and every slash command (`--write` saves
  it as `<profile>/slack-manifest.json`, `--commands-only` prints the
  command list).
- **Email** polls the inbox over IMAP every `options.poll_interval_secs`
  (15) and answers over SMTP as `options.address` with the password in
  `EMAIL_PASSWORD`. `options.imap_host` and `options.smtp_host` are
  required; `imap_security`/`smtp_security` are `tls` (IMAP default, and
  SMTP on port 465), `starttls` (SMTP default) or `plain` (loopback hosts
  only), and `username` overrides the login name. Mail already in the
  inbox when the adapter first connects is left alone; replies thread
  under the mail they answer with a single `Re:`; mail from the mailbox
  itself, from no-reply or bounce senders, or marked automatic or bulk is
  not answered. Long replies go out whole. Mail counts only when the
  topmost `Authentication-Results` header of `options.authserv_id` (your
  receiving server, for example `mx.google.com`) authenticates its From
  domain, so a spoofed From is dropped; set
  `options.require_authenticated_sender = false` to accept unauthenticated
  mail. `options.tls_verify = false` skips certificate checks (warned at
  every start) and `options.skip_attachments = true` ignores attachments.
  Allowlist entries are whole addresses.
- **Matrix** syncs with `options.homeserver` using `MATRIX_ACCESS_TOKEN`;
  the sync cursor survives reconnects, a two-member room is a direct
  chat, and invites from `allow_users` are accepted. Rooms are not
  end-to-end encrypted yet.
- **Signal** talks to a signal-cli HTTP daemon at `options.http_url` for
  the account `options.account` (E.164). Run the daemon with
  `--receive-mode on-connection` so Signal holds messages while the
  adapter is disconnected; an unreachable daemon is retried with
  `signal_daemon_unreachable` (2 to 60 s apart, with a health check after
  two quiet minutes). Groups are answered only when listed in
  `options.allow_groups` (`["*"]`: every group). Writing to yourself (Note
  to Self) talks to the agent; its answers there are not read back.
  Markdown is sent as Signal text styles.
- **WhatsApp** runs the managed WhatsApp Web bridge runtime for each
  profile (its linked-device session under
  `<profile>/platforms/whatsapp/session/`), or uses a bridge already
  running at `options.bridge_url`. One person is one sender whether
  WhatsApp names them by phone number or LID. A device logged out from the
  phone stops the adapter with `whatsapp_logged_out`. `options.mode` is
  `bot` (a number of its own) or `self_chat` (your number: only your chat
  with yourself counts); `options.group_policy` is `pairing` (no group
  until listed in `allow_chats`), `allowlist` or `open`. Clarify questions
  arrive as polls whose vote is the answer; approvals are numbered and
  answered by number.
- **Webhook** serves each route `[gateway.platforms.webhook.routes.<name>]`
  at `/p/<profile>/webhook/<name>`. A route needs `secret_secret`, a
  secret its senders sign with (GitHub `X-Hub-Signature-256`, GitLab
  `X-Gitlab-Token`, Standard Webhooks `webhook-signature`, or
  `X-Webhook-Signature-V2` with `X-Webhook-Timestamp`), unless it sets
  `insecure_no_auth = true`, which only a loopback listener allows.
  `events` limits the accepted event types and `prompt` renders the turn's
  input from the payload (`{pull_request.title}`, `{__raw__}`). Each
  delivery runs as its own conversation; replies are recorded in
  `<profile>/logs/webhook/<route>.jsonl`.

```toml
[gateway.platforms.webhook.routes.ci]
secret_secret = "CI_WEBHOOK_SECRET"
events = ["pull_request"]
prompt = "Review PR #{pull_request.number}: {pull_request.title}"
```

A route admits `rate_limit_per_min` (30) requests a minute (then 429),
answers a redelivery of the same delivery id within an hour with
`{"status":"duplicate"}`, and refuses bodies over
`gateway.platforms.webhook.max_body_bytes` (1 MiB) with 413 before
reading them. `filters` (`exists`, `missing`, `equals`, `not_equals`,
`contains`, `in`, `in_file`, `regex`, grouped by `all`, `any`, `not`, over
`payload.<path>`, `event` and `headers.<Name>`) drop events that do not
match. `script` runs a transform from the profile's `scripts/` directory
in the sandbox with the payload on stdin: a JSON object it prints
replaces the payload, other output is added as `script_output`, and no
output, `[SILENT]` or `{"ignore": true}` drops the event. `skills` and
`toolsets` shape each delivery's session. With `deliver_only = true` no
model runs: the rendered prompt goes straight to `deliver` (a send
target such as `telegram`).

```toml
[gateway.platforms.webhook.routes.alerts]
secret_secret = "ALERTS_SECRET"
deliver_only = true
deliver = "telegram"
prompt = "ALERT {alert.name} on {alert.host}"
filters = { equals = { path = "payload.alert.severity", value = "critical" } }
```

`prometheus plugins test-adapter <platform>` runs the adapter conformance
kit (connect and reconnect, descriptor, normalization, deduplication,
splitting at the declared limit, retry classification, cards and every
optional capability: edit, typing, reactions, threads, delete, inbound and
outbound media with one file per declared kind, streamed reply audio)
against a built-in mock of any first-party platform, prints one `pass`,
`FAIL` or `skip` line per check (an undeclared capability is skipped with
its reason) and exits 0 only when none failed. Where a bot cannot post
voice notes (Slack, Signal) a voice reply arrives as an audio file;
WhatsApp's bridge sends every audio file as a voice note.

The first-party mocks are developer and CI tooling: release binaries leave
them out (they would push the binary past its size budget), so there the
command answers `surface.not_included` (69) for a first-party name. Build
with `cargo build -p prometheus --features adapter-kits` (as
`scripts/check` and CI do) to run them.

What the gateway does for every adapter:

- Long replies are split at paragraph, line and word boundaries (never
  inside inline code), code fences are closed and reopened with their
  language, and parts end with ` (i/N)`; more than `gateway.delivery.
  max_parts` (10) parts become the first part plus the full text attached
  as a document.
- A redelivered event within `gateway.dedup.ttl_secs` (300) is a
  duplicate; text split by the user's client is joined by `batching`
  (on by default for WhatsApp).
- The agent writes Markdown; each platform gets its own markup (Telegram
  `MarkdownV2`, Slack `mrkdwn`, WhatsApp style, Matrix HTML, Signal text
  styles, and `text (url)` links where only plain text is shown). Code is
  never changed.
- Where `gateway.platforms.<name>.reactions` is on (by default on Discord
  and Slack), the message that started a turn gets 👀, then ✅ or ❌.
- A final reply naming a file by path (outside code) delivers the file:
  images and video inline, audio as audio or a voice note, documents and
  archives as uploads (`gateway.delivery.detect_paths`, default on).
  `[[as_document]]` anywhere in a reply sends every attachment as a
  document. A path inside the session's container is read from its
  mounted host directory.
- Sends failing transiently are retried twice (2 s backoff with jitter);
  a platform asking to wait more than a minute leaves the message in the
  outbox for later; a reply whose markup the platform refuses is resent
  once as plain text.
- Inbound files are downloaded into the profile's `cache/media/` under
  `gateway.media.max_inbound_bytes` (128 MiB) and deleted after
  `gateway.media.cache_max_age_secs` (a day) unless a pinned session
  mentions them. The agent attaches files with `MEDIA:<path>` lines
  (`[[as_document]]`, `[[as_voice]]`), delivered only from the session's
  workspace, the media cache or the blob store; anything else is withheld
  with a note.
- Voice notes are transcribed with the profile's speech provider
  (`gateway.voice`); replies are spoken back to voice notes by default
  (`reply_mode = "mirror"`).
- Approval cards show the tool, the exact action, the session's working
  directory, the risk reasons and when the request expires (unanswered,
  it is denied). They use buttons where the platform has them (signed
  per daemon start, so a forged button press decides nothing), a
  numbered list answered with `1`..`4` on `numbered` platforms, and
  `/approve <id> <choice>` elsewhere. Clarify questions list their
  choices by number everywhere and, on platforms with buttons, as
  buttons too; the asker answers with a button, the numbers or text. A
  question taking several answers says "Multiple selections allowed" and
  takes `1, 3`, the option texts or free text (a select menu on Discord,
  checkboxes on Slack); a number out of range is answered with the valid
  range.
- A bot token used by two profiles or homes connects once: the second
  adapter reports `fatal` with `<platform>_lock`, naming the holder and
  the command that stops it; `prometheus daemon run --replace` takes the
  credential over.
- Webhook-style adapters are served on one listener, `daemon.toml
  gateway.http.listen` (default `127.0.0.1:8644`), at
  `/p/<profile>/<platform>/`; an unsigned request gets 401. A
  non-loopback address needs `gateway.http.public = true`.

## Who may talk to the agent

Nobody, by default. A sender is admitted by the first rule that matches:
the relay's upstream verdict, `allow_chats`, `allow_all`, an approved
pairing, `allow_users`, then the adapter's own allowlist. An adapter's
own policy counts only in `allowlist` mode and only when its sender check
answers `allow`; `deny`, any other answer, a failure or no answer within
5 seconds leave the default deny. Wildcards in
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
platforms that can send statelessly (Telegram, Discord) send directly with
the profile's token and the others need the daemon (exit 69). Here the
token was never stored, so the direct send fails:

```console
$ prometheus gateway send --to telegram:-1001234567890 "RAM 92%" # exits 1
error: not delivered to telegram:-1001234567890: refused: secret TELEGRAM_BOT_TOKEN is missing from profile 'default'; set it with `prometheus -p default secrets set TELEGRAM_BOT_TOKEN`
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

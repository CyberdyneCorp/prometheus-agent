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
  answered by number. Locations and live locations arrive as structured
  attachments the model reads as `[Location: <name> (<lat>, <lon>)]`; a
  reply line `LOCATION: <lat>,<lon> [name]` is sent as a location pin
  (other platforms get a map link instead).

  Link a profile with `prometheus -p <profile> gateway whatsapp pair`: it
  installs the bridge runtime when needed (the pinned Node runtime, then
  the bridge shipped with Prometheus, adapted from the MIT-licensed Hermes
  Agent bridge, whose packages `npm ci` installs from its lockfile), then
  draws the linking QR code in the terminal and refreshes it until the
  phone links (WhatsApp > Settings > Linked devices), at most 120 seconds
  (exit 75). A terminal narrower than 60 columns is refused (exit 2); use
  `--pairing-code <phone>` to link by typing a code on the phone instead.
  Pairing replaces the profile's earlier session (kept mode 0700).

  ```text
  prometheus -p work gateway whatsapp pair
  prometheus -p work gateway whatsapp pair --pairing-code +5511999999999
  ```
- **Webhook** serves each route `[gateway.platforms.webhook.routes.<name>]`
  at `/p/<profile>/webhook/<name>`. A route needs `secret_secret`, a
  secret its senders sign with (GitHub `X-Hub-Signature-256`, GitLab
  `X-Gitlab-Token`, Standard Webhooks `webhook-signature`, or
  `X-Webhook-Signature-V2` with `X-Webhook-Timestamp`), unless it sets
  `insecure_no_auth = true`, which only a loopback listener allows.
  `events` limits the accepted event types and `prompt` renders the turn's
  input from the payload (`{pull_request.title}`, `{__raw__}`). Each
  delivery runs as its own conversation, with the read-only
  `webhook-safe` toolset ([`web_search`](../reference/tools.md#web_search), [`web_extract`](../reference/tools.md#web_extract), [`vision_analyze`](../reference/tools.md#vision_analyze),
  and [`clarify`](../reference/tools.md#clarify), which is answered as unavailable since nobody can reply)
  unless the route lists `toolsets` (only ones the profile enables; no
  terminal, file, browser or code execution by default). Where the answer
  goes is `deliver`:
  - `origin` (the default): the HTTP caller waits up to
    `origin_wait_secs` (30) and gets 200
    `{"status":"completed",...,"answer":"..."}`; a longer run is answered
    202 `accepted` and its answer goes to the log;
  - `log`: recorded in `<profile>/logs/webhook/<route>.jsonl` (202 at
    once);
  - `github_comment`: a comment on the payload's pull request or issue,
    posted with the token in `github_token_secret`
    (`options.github_api_url` for GitHub Enterprise);
  - a send target `platform[:chat_id[:thread_id]]` of the profile.

  `deliver_extra` targets get a copy, and `mirror_to_session = true`
  appends the delivered text to each target chat's session as
  `[Webhook delivery: <route>]` (an attributed assistant message). With
  `[gateway.mcp_events] enabled = true` (experimental, off by default)
  notifications of MCP servers that declare an `events` route arrive on
  that route as internal events the gateway vouches for (no HTTP, no
  signature), with the same filters, delivery and toolsets.

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
platform (`gateway.platforms.<name>.busy`) and per chat policy. An admin
sets the platform's mode from a chat with `/busy queue|steer|interrupt`
(written to `gateway.platforms.<name>.busy.input_mode` in `config.toml`,
effective from the next message); `/busy status` names the effective mode
and its source, for example "steer (platform override)".

## Chat controls

- **Start pings.** Telegram's `/start`, opening the Slack app's Messages
  tab (`app_home_opened`; subscribe the app to that event, as the
  generated manifest does) and Discord's `/start` command (Discord has no
  app-home event, so the adapter always registers `/start` first) get a
  short greeting naming `/help`; it creates no session, starts no turn and
  does not count against rate limits. Slack sends the event on every open,
  so a user is greeted at most once per redelivery window
  (`gateway.dedup.ttl_secs`).
- **`/commands [page]`** lists every command and skill command the sender
  may run here (their tier applies), 15 per page on Telegram and 20
  elsewhere. Platforms with buttons page with Previous/Next buttons that
  edit the list in place; others show `page <n>/<total>`. A page past the
  end answers "page 3 does not exist (2 pages)". Native command menus keep
  built-ins first, then skills by most recent use, up to the platform's
  menu cap.
- **`/approve [session|always]`, `/deny [all] [reason]`** answer the
  oldest pending approval of the chat's session by text, with the same
  authorization as card buttons ("No pending approval" when there is
  none).
- **`/retry`, `/discard` after a restart.** A turn a crash interrupted is
  `resume_pending`: new messages wait behind it, in order, until `/retry`
  runs it again (then the waiting messages) or `/discard` drops it (its
  partial transcript stays) and runs them now. After
  `gateway.sessions.resume_pending_ttl_secs` (default 86400) it is dropped
  automatically.
- **Stall watchdog.** A turn with no model output, tool progress or tool
  result for `gateway.session_stall_timeout_secs` (default 900; `0` turns
  it off) gets one `No progress for <n> min; send /stop to cancel` notice;
  after twice that time it is interrupted and reported as stalled.
- **Reconnect attention.** An adapter outside `connected` for longer than
  `gateway.reconnect_attention_secs` (default 600) is flagged
  (`adapter.state` with `attention = true`, `[needs attention]` in
  `prometheus gateway status`), and each other connected platform's home
  channel hears about it once per outage.

### Display policy

`[gateway.display]` shapes how a turn looks in a chat; any key can be
overridden for one platform under `[gateway.display.platforms.<name>]`
(the platform value wins, an absent one inherits):

```toml
[gateway.display]
tool_progress = "all"            # off | new | all | verbose | log (Telegram: off)
tool_progress_grouping = "accumulate"   # or "separate"
tool_preview_length = 40
interim_assistant_messages = true
long_running_notifications = true      # "Working — <n> min", every 3 min
busy_ack_detail = false
cleanup_progress = false         # delete progress after a successful reply
tool_progress_command = false    # let chats cycle it with /verbose
runtime_footer = false           # or /footer on|off per chat
suppress_warning_notifications = false
status_phrases = { path = "phrases.txt", mode = "append" }

[gateway.display.platforms.discord]
cleanup_progress = true
```

With `tool_progress = "log"` nothing is shown and each call is appended,
secret-redacted, to `<home>/logs/<profile>/tool_calls.log` (rotated at 5
MiB, 3 backups). The footer (model, context use, directory) is added to
what is sent only, never to the transcript. Muted warnings (failed jobs,
watchdog and adapter notices) are recorded as `suppressed` in the outbox;
approvals, clarify questions and replies are never muted.

A final answer that is exactly `[SILENT]`, `NO_REPLY` (or another silence
token) sends nothing in chatter the platform marks as not addressed to the
bot, and "(nothing to add)" to a person who asked. With
`[gateway.message_timestamps] enabled = true`, sessions created afterwards
show the model each message's send time (`[Tue 2026-04-28 13:40:53 CEST]`,
in `cron.timezone`); the stored text stays clean.

### Groups, bots and roles

In groups, channels and threads a message starts a turn only when it
mentions the bot, replies to it, matches one of `mention_patterns` or is in
a thread the bot takes part in (`[gateway.groups]`, overridable under
`[gateway.groups.platforms.<name>]`); DMs are never gated:

```toml
[gateway.groups]
require_mention = true
mention_patterns = ["^hey prom"]     # case-insensitive, at most 32
free_response_chats = ["C0123"]      # answered without a mention
ignored_chats = ["C0999"]            # never answered
observe_unmentioned = false          # keep the rest as context
history_backfill = true              # Discord and Slack by default
history_backfill_limit = 50

[gateway.bots]
allow_bots = "none"                  # or "mentions" / "all"
require_inline_mention = true
burst_window_ms = 1500
human_user_ids = []
```

With `observe_unmentioned`, unaddressed messages are kept and added to the
session of the chat's next turn as `[name|user_id]` lines, which the model
is told are context, not instructions. A mention-triggered turn on Discord
or Slack gets the channel's messages since the bot's last reply as one
untrusted block (Slack reads `conversations.history`, or
`conversations.replies` in a thread; the app needs the `*:history`
scopes). `gateway.platforms.<name>.allow_roles` admits holders of a role
id, checked right after `allow_users`; a failed role lookup denies:

- Discord: role ids (numbers).
- Slack: user group ids (`S…`, from `usergroups.list`; the app needs
  `usergroups:read`).
- Matrix: power levels. A member holds every level from 0 up to their
  own in the room, so `allow_roles = ["50"]` admits moderators and admins;
  levels above 100 match exactly.

A role name instead of an id fails validation, with the id suggested when
the daemon's adapter can resolve it (Matrix knows `admin` = 100,
`moderator` = 50, `member` = 0). A chat policy with
`thread_id` binds one thread or forum topic, and its `preload_skills` open
each new session there as context, outside the system prompt.

### Adapter operations, restart and update

`/platforms` (alias `/gateway`, also in `prometheus chat` and the TUI) lists
the profile's adapters with their state, error code and pause flag. Admins
run `/platform pause <name>` to stop dispatching an adapter's inbound
messages without disconnecting it (they are acknowledged and dropped;
replies still go out) and `/platform resume <name>` to lift the pause,
reset the reconnect backoff and reconnect at once. The pause is kept in
the profile's `state.db` across restarts; the CLI does the same:

```text
prometheus gateway pause discord
prometheus gateway resume discord
```

`/restart` and `/update` are admin-tier and ask for a confirmation in the
same chat and thread first (buttons, or `/approve` / `/deny`); a denial or
no answer within `approvals.timeout_secs` aborts with nothing changed.
After a confirmed `/restart` the daemon drains and exits 75 for its
supervisor, and the next daemon posts `Gateway restarted (<version>, build
<id>)` in the requesting thread. A restart that cut running turns short
also tells each platform's home channel once, unless
`gateway.platforms.<name>.restart_notification = false`. `/update` needs
`update.allow_from_chat = true` in `daemon.toml` (otherwise "updates from
chat are disabled (update.allow_from_chat = false)"); `/update check` only
looks for a release.

### Models, sessions, topics, background tasks, branches and handoffs

- **`/model`** without arguments shows a picker where the platform has
  buttons: the connected providers, then the chosen provider's models, 8
  per page with Previous/Next, edited in place. Elsewhere it prints a
  numbered list answered with `/model <n>`. A model chosen this way (or
  typed, `/model <provider>/<model>`) switches the session like `/model`
  does and becomes the chat's override in `state.db`: every new session in
  the chat starts on it (it outlives `/new`) until `/model --clear`.
  `/model status` shows the session's model and the override. A
  `--once` switch is not remembered.
- **`/sessions [page]`** lists the sessions started in this chat (and
  thread), 10 per page; **`/resume <n>`** (or a session id of the chat)
  routes the conversation back to one, reopening it if it had ended.
- **DM topics** (Telegram, `options.dm_topics.enabled = true`, the bot in
  threaded mode): each topic of the private chat is its own session and
  the root DM is a lobby that answers anything but a command with `Open or
  create a topic to chat (/topic new <name>)`. `/topic new [name]` opens a
  topic (unnamed ones are called "New chat" and take the session's title
  once it has one), `/topic list` lists the topics with sessions, `/topic
  rename <name>` renames the topic it is sent in, and `/topic restore
  <session-id>` opens a topic continuing an earlier session of the chat.
  `/new` inside a topic starts a new session there. Without topic mode
  (`getMe` says so, or the platform refuses to open a topic) the adapter
  logs it once and the DM stays one session.
- **`/background <prompt>`** (alias `/bg`; also in `prometheus chat` and
  the TUI) runs the prompt in a separate session of the profile (the
  chat's model and toolsets, none of its history) and answers at once with
  "Background task started: ... (id bg_...)"; the chat keeps working. The
  result arrives as "✅ Background task complete" or `❌ Background task
  failed: <reason>` in the chat and thread that asked (a notification in
  the CLI and TUI). A chat runs at most `gateway.background.max_per_chat`
  (default 3) tasks at once ("Too many background tasks running (3)").
  Approvals a task's tools ask for are not shown anywhere and time out
  (denied), so a task runs what needs no approval.
- **`/branch [--here] [name]`** (alias `/fork`) forks the session. Where
  the adapter opens threads (Slack, Discord, Telegram topics, Matrix) the
  fork gets a new thread in the same chat titled `name` (or the session's
  title plus " (branch)") and the original thread keeps the original
  session; with `--here`, or without threads, this chat switches to the
  fork (`/resume` goes back). Refused while a turn runs.
- **`/handoff <platform>[:<chat_id>[:<thread_id>]]`** in `prometheus
  chat` or the TUI moves the session to a chat of the profile through the
  running daemon: the platform's home channel when no chat is named, in a
  new thread or topic where the adapter opens them. The chat must be a
  home channel, an `allow_chats` entry or a chat the profile already talks
  in. The chat gets a replay of the last 20 messages ("You: ..." /
  "Assistant: ..."), the session a notice that it continues there, and the
  next message written there continues the session. The client then exits
  0 printing `continued on <platform>; resume locally with prometheus chat
  --resume <session-id>`. Refused while a turn runs.

### Streaming, task cards and the working status

`[gateway.streaming] transport` picks how an answer streams: `auto` (the
default) uses the platform's own streaming API where the adapter has one
(Slack's `chat.startStream`), otherwise one message edited as the answer
grows (`edit`; `draft` also edits until a platform offers drafts), and
`off` sends only the final answer. A native stream that fails mid-turn
is stopped and the same message is edited from then on, so the answer
appears exactly once. Where the adapter shows task cards (Slack), tool
calls appear as cards in the stream, updated as they finish, instead of
progress bubbles (unless `tool_progress = "off"`); where it has a working
status line (Slack assistant threads) the thread shows "is thinking…"
while the turn runs, if `typing_indicator` is on for the platform.

### One bot for several profiles

A profile whose adapter holds the credential can route matching events
to another served profile:

```toml
# profiles/default/config.toml: the bot's profile
[[gateway.profile_routes]]
name = "acme"
platform = "discord"
guild_id = "123"          # also chat_id, thread_id, user_id
profile = "acme"

# profiles/acme/config.toml: the target must opt in
[gateway]
accept_routes_from = ["default"]
```

The most specific route wins (`user_id` 16, `thread_id` 8, `chat_id` 4,
`guild_id` 2; every field set must match; ties keep declaration order; a
channel route also covers its threads). The receiving profile authorizes
the event; then the target's session keys (`acme:discord@123:...`),
config, memory and toolsets apply, and the reply leaves through the
receiving profile's bot. Unmatched events stay. Validation refuses an
empty id (`user_id = ""` would widen the route) and a target whose
`accept_routes_from` does not name the receiving profile.

### Concurrency caps

`daemon.toml` bounds what runs at once across every profile and surface:

```toml
[gateway.limits]
max_concurrent_sessions = 4     # turns running at once; 0 = unbounded (default)
max_live_client_sessions = 16   # detached TUI/desktop/dashboard sessions kept loaded
```

A turn beyond `max_concurrent_sessions` waits first in, first out and its
chat (or terminal) is told `Waiting for a free slot (<n> ahead)`; it
starts when a running turn ends. Detached client sessions beyond
`max_live_client_sessions` are unloaded, least recently attended first
(never one with a running turn or an attached client), and resumed from
storage when reopened.

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

# Platform Adapters

## Purpose
Platform adapters connect the messaging gateway to individual chat services through one typed contract: an adapter implements a small set of required operations and declares everything else (message length limit and unit, media kinds, editing, buttons, threads, receipts, streaming audio) in a single capability descriptor that is the only source of truth the gateway consults. Telegram, Discord, Slack, Email, Matrix, Signal, WhatsApp and Webhook ship first-party in the binary; every other platform arrives as a sandboxed WebAssembly adapter plugin implementing the same `platform-adapter` world, while shared behaviour (splitting, retry, deduplication, media safety, approval rendering) lives in the host so adapters stay thin.

## ADDED Requirements

### Requirement: Adapter contract
Every adapter, first-party or plugin, SHALL implement the required operations `connect`, `disconnect`, `send(chat, content, reply_to, metadata) -> send-result` and `chat-info(chat) -> {name, chat_type}` with `chat_type` in `dm`, `group`, `channel`, `thread`, and SHALL return a capability descriptor from `describe()` before `connect`; a reconnect SHALL preserve a server-side update queue or cursor where the platform has one so no inbound message is skipped.
**Milestone:** M2

#### Scenario: Plugin missing a required export
- **WHEN** a WASM adapter component does not export `send`
- **THEN** the system SHALL refuse to load it with an error naming the missing operation

#### Scenario: Reconnect does not drop updates
- **GIVEN** the Telegram adapter loses its connection while 3 messages arrive
- **WHEN** it reconnects
- **THEN** the system SHALL receive all 3 messages exactly once

### Requirement: Capability descriptor is the single source of truth
The capability descriptor SHALL declare `platform`, `max_message_length` (positive integer, required), `length_unit` in `chars`, `utf16`, `utf8_bytes`, `splits_long_messages`, `supports_edit`, `supports_delete`, `supports_typing`, `supports_reactions`, `supports_threads`, `interactive` in `native`, `numbered`, `none`, `media_in` and `media_out` (sets of `image`, `audio`, `voice`, `video`, `document`), `delivery_receipts`, `read_receipts`, `streaming_tts`, `markup` in `plain`, `markdown`, `html`, `platform`, and `binds_http`, plus the optional boolean flags (default false) whose behaviour other capabilities define: `can_fetch_history`, `exposes_roles`, `dm_topics`, `native_stream`, `task_cards`, `status_line` (`messaging-gateway`), `stateless_send` (`agent-interop`), and the per-message marker `not_addressed_to_bot`, and the optional `continuable_surface` in `thread` (default) or `in_channel` (`scheduled-jobs`); the gateway SHALL read limits and capabilities only from the descriptor, a descriptor missing a required field or with a non-positive `max_message_length` SHALL fail adapter load, and `prometheus gateway platforms` SHALL print each loaded adapter's descriptor.
**Milestone:** M2

#### Scenario: Missing limit fails load
- **WHEN** a plugin adapter's descriptor omits `max_message_length`
- **THEN** the system SHALL refuse to load it with "descriptor field max_message_length is required"

#### Scenario: One limit everywhere
- **GIVEN** the Email adapter declares `max_message_length = 50000`
- **WHEN** a 30,000-character reply is delivered and `prometheus gateway platforms` is run
- **THEN** the system SHALL send one message and SHALL report 50000 as the Email limit

### Requirement: Optional capabilities degrade in the host
The gateway SHALL call an optional operation only when the descriptor declares it and SHALL degrade otherwise: no `supports_edit` sends streamed finals and card updates as new messages, no `supports_typing` skips typing indicators, `interactive = "numbered"` renders choices as a numbered list answered by replying with the number, `interactive = "none"` renders slash-command instructions, and a declared operation that returns `unsupported` at runtime SHALL be treated as undeclared for the rest of that connection and logged once.
**Milestone:** M2

#### Scenario: Edit unsupported
- **GIVEN** an adapter with `supports_edit = false`
- **WHEN** an approval is resolved
- **THEN** the system SHALL send a new message stating the outcome instead of editing the card

### Requirement: First-party platforms and typed platform names
The binary SHALL ship first-party adapters `telegram`, `discord`, `slack`, `email`, `matrix`, `signal`, `whatsapp` and `webhook`, plus the internal `local` (attached clients) and `relay` platforms; any other platform name SHALL be accepted only when an installed, enabled adapter plugin declares it, platform names SHALL be case-normalized to lower case, and a `gateway.platforms.<name>` table naming an unknown platform SHALL fail validation with "unknown platform '<name>'; install an adapter plugin that provides it".
**Milestone:** M2

#### Scenario: Unknown platform table
- **WHEN** `config.toml` contains `[gateway.platforms.myspace]` with no plugin providing it
- **THEN** the system SHALL fail validation with the unknown-platform error

#### Scenario: Plugin platform accepted
- **GIVEN** an installed plugin whose `plugin.toml` declares a `platform-adapter` named `Mattermost`
- **WHEN** `[gateway.platforms.mattermost]` is configured
- **THEN** the system SHALL load the plugin's adapter for it

### Requirement: Default first-party message limits
The first-party descriptors SHALL declare: `telegram` 4096 `utf16`; `discord` 2000 `chars`; `slack` 39000 `chars`; `email` 50000 `chars` with `splits_long_messages = true`; `matrix` 32000 `utf8_bytes`; `signal` 8000 `chars`; `whatsapp` 4096 `chars`; `webhook` 65536 `utf8_bytes`; `local` and `relay` SHALL take their limit from the client or connector descriptor; a user MAY lower but never raise a limit with `gateway.platforms.<name>.max_message_length`.
**Milestone:** M2

#### Scenario: Raising a limit is rejected
- **WHEN** `gateway.platforms.discord.max_message_length = 4000`
- **THEN** the system SHALL fail validation stating Discord's maximum is 2000

#### Scenario: Telegram counts UTF-16
- **WHEN** a reply of 4000 characters containing 200 astral-plane emoji is sent to Telegram
- **THEN** the system SHALL split it so each part is at most 4096 UTF-16 code units

### Requirement: Platform configuration shape
Each `[gateway.platforms.<name>]` table SHALL accept the common keys `enabled` (default `false`), `home_channel`, `reply_to_mode` in `off`, `first` (default), `all`, `typing_indicator` (default `true`), `max_message_length`, `allow_users`, `allow_chats`, `allow_all`, `admins`, `group_admins`, `user_commands`, `unauthorized_dm`, `busy`, `streaming`, `rate_limit`, `sessions`, and an `options` table validated against the adapter's own published config schema; credentials SHALL be referenced by secret name (for example `token_secret = "TELEGRAM_BOT_TOKEN"`), never by value, and any unknown key SHALL fail validation.
**Milestone:** M2

#### Scenario: Inline token rejected
- **WHEN** `gateway.platforms.telegram.token = "123:ABC"` is configured
- **THEN** the system SHALL fail validation stating credentials belong in the secrets store and are referenced by `token_secret`

#### Scenario: Adapter option validated
- **WHEN** `gateway.platforms.email.options.imap_port = "abc"`
- **THEN** the system SHALL fail validation naming `gateway.platforms.email.options.imap_port`

### Requirement: Scoped credential access
Adapters SHALL obtain credentials only through the owning profile's ProfileScope secrets handle, and a WASM adapter SHALL be able to read only the secret names it declared in `plugin.toml` and the user granted; a missing secret SHALL put the adapter in state `fatal` with code `missing_secret` naming the secret, and no adapter SHALL read process environment variables.
**Milestone:** M2

#### Scenario: Secondary profile secret miss
- **GIVEN** profile `work` enables Telegram but its secrets store has no `TELEGRAM_BOT_TOKEN` while the default profile's does
- **WHEN** `work`'s adapter connects
- **THEN** the system SHALL report `missing_secret` for `work` and SHALL NOT use the default profile's token

### Requirement: Inbound event normalization
Adapters SHALL submit inbound traffic as typed inbound events carrying platform message id, canonical sender id, sender display name, chat id, chat type, optional scope id and thread id, timestamp, text, attachments, reply-to id and an `is_bot` flag, and SHALL canonicalize platform id aliases (for example WhatsApp phone-number and LID identifiers of one user, Discord channel-initiating messages keyed on the prospective thread id) before submission so one human maps to one sender id.
**Milestone:** M2

#### Scenario: WhatsApp alias flip
- **WHEN** the same WhatsApp user appears once by phone JID and once by LID
- **THEN** both events SHALL carry the same canonical sender id and route to one session

### Requirement: Typed admission result
Submitting an inbound event SHALL return exactly one admission result in `accepted`, `duplicate`, `unauthorized`, `pairing_issued`, `rate_limited`, `queued`, `dropped`; only `accepted` and `queued` SHALL count as delivered for internal-event retry accounting, and an internal event that would displace a pending human follow-up SHALL return `dropped` and be retried later.
**Milestone:** M2

#### Scenario: Duplicate reported
- **WHEN** Slack redelivers an event already admitted
- **THEN** the system SHALL return `duplicate` and start no turn

### Requirement: Inbound deduplication and text batching in the host
The host SHALL drop repeated inbound events keyed by `(profile, platform, chat id, platform message id)` within a window of `gateway.dedup.ttl_secs` (default 300) holding at most `gateway.dedup.max_entries` per adapter (default 2000), and SHALL batch rapid consecutive text from one sender when `gateway.platforms.<name>.batching.delay_ms` is set (default 300, maximum 2000) with `split_delay_ms` (default 1000, maximum 4000, at least `delay_ms`); out-of-range values SHALL fail validation rather than be clamped.
**Milestone:** M2

#### Scenario: Batch of a split paste
- **GIVEN** `delay_ms = 300`
- **WHEN** a user's client splits a long paste into 3 messages arriving 100 ms apart
- **THEN** the system SHALL start one turn with the three texts joined in order

#### Scenario: Out of range delay
- **WHEN** `batching.delay_ms = 5000`
- **THEN** the system SHALL fail validation naming the 2000 ms maximum

### Requirement: Fence-aware message splitting
The host SHALL split a reply longer than the effective limit (measured in the descriptor's `length_unit`) by preferring paragraph, then line, then word boundaries, SHALL never split inside an inline code span or a multi-unit character, SHALL close an open code fence at a part's end and reopen it with the same language tag in the next part, SHALL append ` (i/N)` to every part when more than one is produced (the indicator counted within the limit), and SHALL terminate for any limit of at least 16 units; adapters with `splits_long_messages = true` SHALL receive the whole reply unsplit.
**Milestone:** M2

#### Scenario: Code fence carried across parts
- **WHEN** a reply with a 5000-character ```python block is sent to Discord
- **THEN** each part SHALL be at most 2000 characters, end with a closing fence, and the next part SHALL reopen with ```python

#### Scenario: Pathological limit terminates
- **WHEN** a user lowers a limit to 16 and sends a 1000-character word
- **THEN** the system SHALL produce finitely many parts each within 16 units

### Requirement: Oversized output handling
When splitting a reply would exceed `gateway.delivery.max_parts` parts (default 10), the host SHALL send the first part followed by a final part ending "... [truncated, full output attached]" and SHALL attach the full text as a `document` when the descriptor's `media_out` includes `document`, otherwise store it in the blob store and append "full output saved as <blob id>; run `prometheus sessions show <session_id>`"; adapters with `splits_long_messages = true` SHALL receive the full text.
**Milestone:** M2

#### Scenario: Long cron output to Telegram
- **WHEN** a cron job produces 120,000 characters for Telegram
- **THEN** the system SHALL send a truncated message plus the full output as a document

### Requirement: Send retry semantics
The host SHALL retry a send that fails with a typed transient error (`rate_limited`, `network`, `server_error`) up to 2 times inline with exponential backoff from 2 seconds plus up to 1 second of jitter, SHALL honour a platform `retry_after` up to 60 seconds inline and hand longer waits to the outbox without sleeping, SHALL NOT retry a timeout that occurred after the request was transmitted (marking it `unknown`), and SHALL resend once as plain text when the adapter reports a permanent `formatting` error.
**Milestone:** M2

#### Scenario: Long flood wait handed off
- **WHEN** Telegram answers with `retry_after = 300`
- **THEN** the system SHALL return immediately and the outbox SHALL schedule the send after 300 seconds

#### Scenario: Markup rejected
- **WHEN** Telegram rejects a reply's HTML entities as malformed
- **THEN** the system SHALL resend it once as plain text

### Requirement: Inbound media limits and caching
Inbound media SHALL be capped at `gateway.media.max_inbound_bytes` (default 134217728; `0` disables; negative fails validation), enforced against both the declared length and the running byte count while streaming, with an oversize download aborted and the user told "Attachment too large (<n> MB > <limit> MB)"; accepted media SHALL be stored content-addressed in the owning profile's `cache/media/` and deleted after `gateway.media.cache_max_age_secs` (default 86400) unless referenced by a session marked keep.
**Milestone:** M2

#### Scenario: Lying content length
- **GIVEN** a server declares 1 MB but streams 200 MB
- **WHEN** the download passes the cap
- **THEN** the system SHALL abort it, write nothing to the cache and notify the user

### Requirement: Outbound attachments and path safety
The agent SHALL attach outbound media through structured reply attachments or `MEDIA:<path>` directives (deduplicated, ignored inside code blocks, inline code, blockquotes and quoted JSON, with `[[as_voice]]` and `[[as_document]]` modifiers), and the host SHALL deliver a local path only when its fully resolved real path lies inside the session's workspace, the profile's `cache/media/` or the blob store, refusing symlink escapes, the secrets store, `config.toml`, `state.db` and any path outside those roots with a visible note "attachment <name> withheld: path not allowed".
**Milestone:** M2

#### Scenario: Secrets file refused
- **WHEN** a reply contains `MEDIA:~/.prometheus/profiles/default/secrets.toml`
- **THEN** the system SHALL not upload it and SHALL append the withheld note

#### Scenario: Symlink escape refused
- **GIVEN** `workspace/out.png` is a symlink to `/etc/passwd`
- **WHEN** it is attached
- **THEN** the system SHALL refuse it

### Requirement: Voice input transcription
When `gateway.voice.stt_enabled` is true (default) the gateway SHALL transcribe inbound voice and audio clips with the profile's configured speech-to-text provider and prepend the transcript to the user's text, echoing the transcript back once when `gateway.voice.echo_transcripts` is true (default `true`); when disabled or when transcription fails it SHALL instead give the agent the note "[The user sent a voice message: <path> (duration: <s>s)]".
**Milestone:** M2

#### Scenario: Transcription failure is explicit
- **GIVEN** the STT provider returns an error
- **WHEN** a voice note arrives
- **THEN** the agent input SHALL contain the voice-message note with the cached path and the turn SHALL proceed

### Requirement: Voice replies and streaming TTS
`gateway.voice.reply_mode` SHALL accept `off`, `mirror` (default; reply with voice when the user's message was voice) or `always`; adapters declaring `streaming_tts` SHALL receive audio through `begin`, `write`, `finish` and `abort` operations while the reply streams, and a turn whose streamed audio completed SHALL NOT also receive a whole-file voice reply.
**Milestone:** M2

#### Scenario: No double playback
- **GIVEN** an adapter with `streaming_tts` and `reply_mode = "mirror"`
- **WHEN** the streamed audio for a turn finishes
- **THEN** the system SHALL NOT send an additional voice file for that turn

### Requirement: Typing indicator and human delay
When `typing_indicator` is true the host SHALL refresh the platform typing indicator every 2 seconds during a turn, bounding each call to between 0.25 and 1.5 seconds so a slow call never stalls the cadence and ignoring call failures, and SHALL insert a uniformly random delay within `gateway.platforms.<name>.human_delay_ms = [min, max]` (default none) before each outbound part.
**Milestone:** M2

#### Scenario: Slow typing call
- **GIVEN** `send_typing` takes 5 seconds to fail
- **WHEN** a turn runs for 10 seconds
- **THEN** the system SHALL still issue a typing refresh every 2 seconds

### Requirement: Interactive approval cards on every platform (upgrade)
Every adapter SHALL render approval, clarify and confirmation prompts through the host's card model, which carries a title, the exact command or action, working directory, risk reason, choices (`once`, `session`, `always`, `deny` for approvals) and expiry time: `native` adapters SHALL use platform buttons (Telegram inline keyboards, Discord components, Slack Block Kit, Matrix and WhatsApp interactive replies where available), `numbered` adapters (Signal, Email, SMS-style plugins) SHALL accept a reply of the choice number or keyword quoting the prompt id, and `none` SHALL accept `/approve <id> <choice>` and `/deny <id>`; callback data SHALL be `appr:<id>:<choice>:<mac>` where `<mac>` is an HMAC over id and choice with a per-boot key, and a callback with an invalid MAC or for an unknown or already-resolved id SHALL be rejected and logged.
**Milestone:** M2

#### Scenario: Signal numbered approval
- **GIVEN** a pending approval rendered on Signal as "1 Once / 2 Session / 3 Always / 4 Deny"
- **WHEN** the requester replies "1"
- **THEN** the system SHALL approve once and confirm "Approved once"

#### Scenario: Forged callback
- **WHEN** a Telegram callback arrives with data `appr:42:always:deadbeef` whose MAC does not verify
- **THEN** the system SHALL reject it and the approval SHALL remain pending

#### Scenario: Card updated after resolution
- **GIVEN** a Slack approval card
- **WHEN** it is resolved from any surface
- **THEN** the card SHALL be edited to show the outcome and resolver and its buttons removed

### Requirement: Adapter access-policy delegation
An adapter MAY declare `enforces_own_access_policy` with an effective mode in `allowlist` or `open`, and the gateway SHALL trust its verdict without a configured allowlist only when the mode is `allowlist`; only the `relay` adapter MAY declare `authorization_is_upstream`; a sender-authorization check returning anything other than `allow` or `deny`, or failing, SHALL be treated as `unknown` and fall through to the gateway's default deny.
**Milestone:** M2

#### Scenario: Open adapter policy not trusted
- **GIVEN** an adapter reporting own policy mode `open` and no allowlist configured
- **WHEN** an unknown sender writes
- **THEN** the system SHALL deny the message

### Requirement: Credential locks across homes
An adapter connecting with a unique platform credential SHALL acquire a machine-global lock keyed by platform and a SHA-256 of the credential identity in the user runtime directory, released on disconnect and only by the holding process; a live foreign holder SHALL put the adapter in state `fatal` with code `<platform>_lock`, retryable, and a message naming the holding home and profile and the remedy `prometheus -p <profile> gateway stop`, and only `prometheus daemon run --replace` MAY take over a live holder, at most once per connect.
**Milestone:** M2

#### Scenario: Same bot token in two profiles
- **GIVEN** profiles `default` and `work` both configure the same Telegram token
- **WHEN** the daemon starts both adapters
- **THEN** the second SHALL fail with `telegram_lock` naming the first profile and the first SHALL stay connected

### Requirement: Shared HTTP listener for webhook-style platforms
Adapters with `binds_http = true` (webhook, WhatsApp Cloud-style, Teams-style and similar plugins) SHALL NOT open sockets themselves; the daemon SHALL serve them on one listener `gateway.http.listen` (default `127.0.0.1:8644`) at paths `/p/<profile>/<platform>/...`, SHALL require each such adapter to verify the platform's request signature or shared secret before admission (unsigned requests rejected with HTTP 401), and SHALL refuse a non-loopback `gateway.http.listen` unless `gateway.http.public = true`, logging a WARN on every start when public.
**Milestone:** M2

#### Scenario: Two profiles share the listener
- **GIVEN** profiles `default` and `work` both enable `webhook`
- **WHEN** the daemon starts
- **THEN** the system SHALL serve `/p/default/webhook/` and `/p/work/webhook/` on one port

#### Scenario: Unsigned webhook
- **WHEN** a POST to `/p/default/webhook/` lacks a valid signature
- **THEN** the system SHALL respond 401 and admit nothing

### Requirement: Adapter state reporting and reconnect
Each adapter instance SHALL report one state in `disabled`, `connecting`, `connected`, `degraded`, `retrying`, `fatal`, with an error code and message for `degraded`, `retrying` and `fatal`; `degraded` SHALL mean connected but with an unconfirmed send path, retryable failures SHALL reconnect with exponential backoff from 1 second to 300 seconds, and states SHALL be visible in `prometheus gateway status`, the `adapter.state` event and health metrics.
**Milestone:** M2

#### Scenario: Degraded is visible
- **GIVEN** an adapter that connected but whose first send has not succeeded
- **WHEN** `prometheus gateway status` runs
- **THEN** it SHALL show `degraded` with "connected but not yet confirmed active; recovering in background"

### Requirement: WASM adapter plugin isolation
A WASM adapter plugin SHALL run with only the capability grants approved for it: outbound network limited to the hosts declared in `plugin.toml`, secrets limited to declared names, no filesystem access beyond a per-plugin scratch directory under the owning profile, inbound HTTP only through routes on the shared listener, and CPU and memory bounded per call; a trap, panic or limit violation SHALL put only that adapter instance into `retrying` (and `fatal` after 5 crashes within 10 minutes) without affecting other adapters, sessions or the daemon.
**Milestone:** M2

#### Scenario: Undeclared host blocked
- **WHEN** an adapter plugin that declared `api.example.chat` tries to connect to `exfil.example.net`
- **THEN** the system SHALL deny the connection and log a capability violation naming the plugin

#### Scenario: Crash is contained
- **WHEN** an adapter plugin traps while handling an inbound event
- **THEN** the system SHALL restart that adapter with backoff and other adapters SHALL keep delivering

### Requirement: Adapter conformance kit (upgrade)
`prometheus plugins test-adapter <path>` SHALL run the adapter conformance suite against a built-in mock platform server, exercising connect and reconnect, descriptor validity, inbound normalization and deduplication, splitting at the declared limit and unit, retry classification, each declared optional capability, card rendering for the declared `interactive` tier, and capability-grant adherence, printing a pass or fail line per check and exiting 0 only when all pass; the plugin catalog SHALL require a passing report for a `platform-adapter` plugin's listed version.
**Milestone:** M2

#### Scenario: Wrong unit detected
- **GIVEN** an adapter declaring `length_unit = "chars"` whose mock platform enforces UTF-16
- **WHEN** the suite sends an emoji-heavy message at the limit
- **THEN** the kit SHALL fail the splitting check and exit non-zero

### Requirement: Delivery and read receipts (upgrade)
Adapters declaring `delivery_receipts` or `read_receipts` SHALL report receipt events with the platform message id they returned from `send`, and the host SHALL map them to outbox entries; a receipt for an unknown message id SHALL be ignored, and adapters SHALL NOT synthesize receipts the platform does not provide.
**Milestone:** M3

#### Scenario: Signal read receipt
- **GIVEN** a reply sent on Signal with platform id `1700000000123`
- **WHEN** Signal reports it read
- **THEN** the corresponding outbox entry SHALL move to `read`

### Requirement: Busy acknowledgement via reactions (upgrade)
When `gateway.busy.ack` is true and the descriptor declares `supports_reactions`, the host SHALL acknowledge a follow-up received mid-turn by adding the `gateway.platforms.<name>.busy_reaction` reaction (default `eyes`) to that message instead of sending a text acknowledgement, and SHALL add `gateway.ack_reaction` (default none) to every accepted inbound message when configured.
**Milestone:** M3

#### Scenario: Reaction instead of noise
- **GIVEN** Discord and `gateway.busy.ack = true`
- **WHEN** a follow-up arrives during a turn
- **THEN** the system SHALL react with `eyes` and SHALL NOT post a busy text message

### Requirement: First-party adapter transports and required settings
Each first-party adapter SHALL use the following transport and required settings, validated at load with the missing item named: `telegram` Bot API long polling by default or webhook mode, secret `TELEGRAM_BOT_TOKEN`; `discord` Gateway WebSocket plus REST, secret `DISCORD_BOT_TOKEN`, requiring the Message Content privileged intent (connection refused with "enable the Message Content intent" otherwise); `slack` Socket Mode, secrets `SLACK_BOT_TOKEN` and `SLACK_APP_TOKEN`; `email` IMAP polling every `options.poll_interval_secs` (default 15) plus SMTP, secret `EMAIL_PASSWORD` and options `address`, `imap_host`, `smtp_host`; `matrix` client-server `/sync`, secret `MATRIX_ACCESS_TOKEN` and option `homeserver`; `signal` an external signal-cli HTTP daemon at `options.http_url` (inbound SSE `GET /api/v1/events`, outbound JSON-RPC 2.0 `POST /api/v1/rpc`, health `GET /api/v1/check`) with option `account` in E.164; `whatsapp` the WhatsApp Web multi-device protocol through a managed bridge runtime under `<home>/runtimes/` (the Cloud API is the separate `whatsapp_cloud` adapter); `webhook` routes on the shared HTTP listener.
**Milestone:** M2

#### Scenario: Signal daemon unreachable
- **GIVEN** `gateway.platforms.signal.options.http_url = "http://127.0.0.1:8080"` with no daemon running
- **WHEN** the adapter connects
- **THEN** it SHALL enter `retrying` with code `signal_daemon_unreachable` naming the URL

#### Scenario: Email missing host
- **WHEN** email is enabled without `options.imap_host`
- **THEN** validation SHALL fail naming `gateway.platforms.email.options.imap_host`

### Requirement: Telegram webhook mode
`gateway.platforms.telegram.options.mode` SHALL accept `polling` (default) or `webhook`; in webhook mode the adapter SHALL serve updates on the shared listener at `/p/<profile>/telegram/`, register `setWebhook` with `options.webhook_public_url` and the secret named by `options.webhook_secret_secret`, and reject any update whose `X-Telegram-Bot-Api-Secret-Token` header does not match in constant time; enabling webhook mode without the secret SHALL fail validation, and switching back to polling SHALL call `deleteWebhook` before polling.
**Milestone:** M2

#### Scenario: Missing webhook secret
- **WHEN** `mode = "webhook"` is set without `webhook_secret_secret`
- **THEN** validation SHALL fail stating webhook mode requires a secret token

#### Scenario: Forged update
- **WHEN** a POST arrives at the Telegram webhook path with a wrong secret-token header
- **THEN** the system SHALL respond 401 and admit nothing

### Requirement: Telegram topics and per-topic skills
The Telegram adapter SHALL treat each private-chat topic (threaded-mode DM) and each group forum topic as its own thread, keying sessions by `(chat id, thread id)`; `options.dm_topics` and `options.group_topics` SHALL declare per chat a list of topics `{name, thread_id?, icon_color?, skill?}`, the adapter SHALL create declared DM topics lacking a `thread_id` at connect and record the created id in profile state (never by rewriting `config.toml`), SHALL discover topics created elsewhere, and a topic's `skill` SHALL be preloaded into every new session in that topic; `options.ignore_root_dm = true` SHALL ignore ordinary messages in the root DM while still accepting slash commands, and a chat without threaded mode SHALL log "chat is not a forum" once and skip topic creation.
**Milestone:** M2

#### Scenario: Topic preloads a skill
- **GIVEN** a DM topic `Research` declared with `skill = "arxiv"`
- **WHEN** the user sends the first message in that topic
- **THEN** the new session SHALL start with the `arxiv` skill loaded and be separate from the root DM session

### Requirement: Telegram local Bot API server
`gateway.platforms.telegram.options.api_base_url` and `options.file_base_url` SHALL point the adapter at a self-hosted Bot API server, which SHALL raise the adapter's inbound file limit from 20 MB to 2048 MB, and `options.local_mode = true` SHALL read downloaded files from the absolute path the server returns (failing with "local Bot API file not readable: <path>" on permission or mount errors); the oversize message SHALL state the effective maximum, and `prometheus gateway telegram logout` SHALL call `logOut` on the public server once to migrate the bot.
**Milestone:** M3

#### Scenario: Large file with local server
- **GIVEN** `api_base_url` points at a local Bot API server
- **WHEN** a user sends a 300 MB video
- **THEN** the adapter SHALL accept and download it (subject to `gateway.media.max_inbound_bytes`)

### Requirement: Telegram network resilience
When the Telegram API host fails to connect, the adapter SHALL try `options.fallback_ips` (default seed `149.154.166.110`, `149.154.167.220`, extended by DNS-over-HTTPS discovery bounded by `options.fallback_discovery_timeout_secs`, default 5) with TLS still validated against `api.telegram.org`, SHALL keep the first working path sticky for the connection, and SHALL bound initial connect at 30 seconds; `options.fallback_ips = []` SHALL disable fallback, and `options.force_ipv4 = true` SHALL restrict connections to IPv4.
**Milestone:** M3

#### Scenario: DNS blocked
- **GIVEN** `api.telegram.org` does not resolve on the host network
- **WHEN** the adapter connects
- **THEN** it SHALL connect through a fallback IP with certificate validation for `api.telegram.org`

### Requirement: Adapter proxy settings and environment trust
Every adapter's outbound HTTP and WebSocket traffic SHALL use `gateway.platforms.<name>.proxy_url` (`http`, `https` or `socks5` scheme) when set, else `gateway.proxy_url`, else no proxy; inherited proxy environment variables and OS system proxy settings SHALL be honoured only when `gateway.trust_env = true` (default `false`), the proxy URL including credentials SHALL be a secret reference when it contains a password, and an unsupported scheme SHALL fail validation.
**Milestone:** M2

#### Scenario: Ambient proxy ignored
- **GIVEN** `HTTPS_PROXY` is set in the daemon environment and `gateway.trust_env` is unset
- **WHEN** the Discord adapter connects
- **THEN** it SHALL connect directly without the proxy

### Requirement: Native command menus and slash registration
Adapters with a native command menu SHALL register the gateway-surface slash-command registry plus user-invocable skills at connect and after a registry change: Telegram via `setMyCommands` limited to `options.command_menu.max_commands` (default 60, range 1-100) after applying `options.command_menu.priority` with `priority_mode` in `prepend` (default), `append`, `replace`, and an inline-query picker (`@bot <term>`) that searches every command and skill with no cap and returns nothing to unauthorized users; Discord as application commands with an optional `args` string up to Discord's 100-command limit, skipping extras with one WARN naming them; and `options.slash_commands = false` SHALL disable registration (for follower daemons sharing one bot application).
**Milestone:** M2

#### Scenario: Priority survives the cap
- **GIVEN** 140 commands and skills and `priority = ["new", "stop", "model"]`
- **WHEN** the Telegram menu is registered
- **THEN** it SHALL contain 60 entries beginning with `/new`, `/stop`, `/model`

#### Scenario: Follower does not overwrite
- **GIVEN** a second daemon with `gateway.platforms.discord.options.slash_commands = false`
- **WHEN** it connects with the same application
- **THEN** it SHALL NOT register or remove any application commands

### Requirement: Telegram presentation options
The Telegram adapter SHALL support: `options.notifications` in `important` (default; only final replies, approvals and command confirmations notify, every other send uses silent delivery) or `all`; in-place editing of status messages keyed by `(chat, status key)` that falls back to a new message when an edit fails; pinning the user's triggering message silently during a turn and unpinning it on completion when `options.pin_during_turn = true` (default `false`); setting the bot short description to `options.status_online` / `options.status_offline` (defaults "Online" / "Offline") when `options.status_indicator = true`, writing offline only on clean shutdown; `options.drop_pending_on_cold_boot` (default `true`) dropping the backlog only on the first start of a daemon while reconnects always preserve it; and `options.disable_link_previews` (default `false`).
**Milestone:** M3

#### Scenario: Progress is silent
- **GIVEN** `notifications = "important"`
- **WHEN** a turn sends two status updates and a final reply
- **THEN** only the final reply SHALL be sent without silent delivery

#### Scenario: Backlog replay on request
- **GIVEN** `drop_pending_on_cold_boot = false` and 5 updates queued while the daemon was stopped
- **WHEN** the daemon starts
- **THEN** the system SHALL process the 5 updates in order

### Requirement: Markup conversion per platform
The host SHALL convert the agent's Markdown to each descriptor's `markup` with platform-specific rules, falling back to plain text through the send-retry `formatting` path: Telegram MarkdownV2 escaping with small tables rendered as per-row bullet groups and larger tables as a fenced block, or Telegram rich messages when `options.rich_messages = true` (limit 32768) with fallback to MarkdownV2 on rejection; Slack `mrkdwn`; WhatsApp and Signal-style conversion (`**b**`→`*b*`, `~~s~~`→`~s~`, `# H`→`*H*`, `[t](u)`→`t (u)`, fences kept); Signal native text ranges for bold, italic, code, strikethrough and spoiler; Matrix HTML; and plain text with `[t](u)`→`t (u)` for `plain` platforms (SMS, LINE, IRC, ntfy); conversion SHALL never alter text inside code spans or fences.
**Milestone:** M2

#### Scenario: Table on Telegram
- **WHEN** a reply with a 3-column, 4-row table is sent to Telegram with rich messages off
- **THEN** each row SHALL render as a bullet group with header labels and no raw pipe characters

#### Scenario: Link on SMS
- **WHEN** a reply containing `[docs](https://x.y)` is sent to SMS
- **THEN** the delivered text SHALL contain `docs (https://x.y)`

### Requirement: Discord auto-threading and forum channels
The Discord adapter SHALL, when `options.auto_thread = true` (default), create a thread for each @mention in a text channel and continue the conversation there without further mentions, also for free-response channels only when `options.free_response_auto_thread = true` (default `false`), never in `options.no_thread_channels` or for reply-type messages; the thread name SHALL be the message text with mention tokens removed truncated to 80 characters, renamed to the session title after the first turn only if unchanged; in forum channels every agent-initiated send SHALL create a new post titled from its first line (max 100 characters) with attachments on the starter message, and replies to an existing post SHALL stay in that post.
**Milestone:** M2

#### Scenario: Mention creates a thread
- **WHEN** a user writes "@bot summarize the incident" in `#ops`
- **THEN** the bot SHALL create a thread named "summarize the incident" and answer inside it

#### Scenario: Excluded channel stays inline
- **GIVEN** `#ops` in `no_thread_channels`
- **WHEN** a user mentions the bot there
- **THEN** the bot SHALL reply inline without creating a thread

### Requirement: Processing lifecycle reactions
Adapters declaring `supports_reactions` SHALL, when `gateway.platforms.<name>.reactions` is true (default `true` for Discord, Slack, Feishu and DingTalk; `false` for Telegram, Matrix and Signal), add `eyes` to the triggering message when a turn starts and add `white_check_mark` on success or `x` on failure, replacing the start reaction where the platform permits only one bot reaction; reaction failures SHALL be ignored without affecting delivery.
**Milestone:** M2

#### Scenario: Failed turn marked
- **GIVEN** Discord with reactions on
- **WHEN** a turn ends with a provider error
- **THEN** the triggering message SHALL carry `eyes` and `x`

### Requirement: Slack app manifest generator
`prometheus gateway slack-manifest` SHALL print a Slack app manifest enabling Socket Mode with the required bot scopes and event subscriptions and one native slash command per gateway-surface registry command, accepting `--name` (default "Prometheus"), `--description`, `--long-description TEXT` or `--long-description-file PATH` (mutually exclusive, 175-4000 characters, exit 2 otherwise), `--agent-view` (agent messaging events), `--no-assistant`, `--commands-only` (only the slash-command array) and `--write [PATH]` (default `<profile>/slack-manifest.json`, exit 73 when it cannot be written).
**Milestone:** M2

#### Scenario: Commands only
- **WHEN** the user runs `prometheus gateway slack-manifest --commands-only`
- **THEN** the output SHALL be a JSON array containing one entry per gateway-surface command and nothing else

#### Scenario: Description too short
- **WHEN** `--long-description "short"` is given
- **THEN** the command SHALL exit 2 naming the 175-character minimum

### Requirement: Slack slash commands and thread command prefix
The Slack adapter SHALL dispatch each registry command received as a native slash command, also accept the umbrella `/prometheus <command> [args]` (treating `/prometheus <free text>` as an ordinary message), reply to slash commands ephemerally by replacing a "Running /<cmd>…" placeholder, never fall back to a public post, and chunk long command output up to 5 ephemeral posts followed by a truncation notice; because Slack blocks slash commands in threads, any platform declaring `command_prefix_alias = "!"` (Slack by default) SHALL treat a message whose first token (after an optional leading bot mention) is `!<command>` as that command, checking only the first token.
**Milestone:** M2

#### Scenario: Stop inside a thread
- **WHEN** a user writes "@bot !stop" in a Slack thread
- **THEN** the system SHALL run `/stop` for that thread's session

#### Scenario: Bang later in text is ordinary
- **WHEN** a user writes "please run !stop later" in a thread
- **THEN** the system SHALL treat it as an ordinary message

### Requirement: Slack reaction triggers
`gateway.platforms.slack.options.reaction_triggers` SHALL accept `false` (default; reaction events acknowledged and dropped), `true` (reactions on the bot's own messages start a turn) or a list of emoji names (those reactions on any message start a turn), producing a turn whose text is `reaction:added:<emoji>` with the reacted message as reply context and an optional `reaction_trigger_target` channel or `channel:thread_ts` for the reply; the reacting user SHALL pass the same authorization and channel checks as a message author.
**Milestone:** M3

#### Scenario: Unauthorized reactor
- **GIVEN** `reaction_triggers = ["eyes"]`
- **WHEN** a user not on the allowlist adds `eyes` to a message
- **THEN** the system SHALL start no turn

### Requirement: Slack multiple workspaces
The Slack adapter SHALL accept a list of bot-token secret names in `options.bot_token_secrets` plus workspace tokens installed through OAuth stored in the secrets store keyed by team id, de-duplicate them, verify each with `auth.test` at connect, keep a separate client and bot user id per team, and route each inbound event and reply by its team id; a token failing `auth.test` SHALL put only that workspace into `degraded` with its team id named.
**Milestone:** M3

#### Scenario: Reply goes to the right workspace
- **GIVEN** tokens for teams `T1` and `T2`
- **WHEN** a message arrives from team `T2`
- **THEN** the reply SHALL be sent with `T2`'s token

### Requirement: Peer-agent routing self-test (upgrade)
`prometheus gateway selftest <platform> [--peer-agent]` SHALL replay synthetic inbound events through the configured adapter's admission and mention-gating logic without network access or credentials, including another bot mentioning this agent when `--peer-agent` is given, and SHALL classify each failure into `config`, `platform_connectivity`, `bot_identity` or `routing_logic`, exiting 0 only when every check passes and 1 otherwise.
**Milestone:** M3

#### Scenario: Bot mentions ignored by config
- **GIVEN** Slack configured with bots not allowed
- **WHEN** `prometheus gateway selftest slack --peer-agent` runs
- **THEN** it SHALL report a `config` failure naming the bot-admission setting and exit 1

### Requirement: Email adapter behaviour
The Email adapter SHALL support `options.imap_security` and `options.smtp_security` in `tls` (default for IMAP and SMTP port 465), `starttls` (default SMTP otherwise) and `plain` (loopback hosts only), with certificate verification on unless `options.tls_verify = false` (warned at every start for non-loopback hosts); at first connect it SHALL mark existing mail seen and then poll UNSEEN; inbound it SHALL prefix `[Subject: …]` unless the subject starts with `Re:`, strip HTML, ignore self-sent, `noreply`, `mailer-daemon` and bounce senders and mail with `Auto-Submitted`, `Precedence: bulk` or `List-Unsubscribe`, skip attachment parts before decoding when `options.skip_attachments = true`, and require the topmost `Authentication-Results` header from `options.authserv_id` to authenticate the From domain unless `options.require_authenticated_sender = false`; outbound it SHALL reply in plain UTF-8 with `In-Reply-To` and `References` headers and a single `Re:` prefix, and allowlist entries SHALL be whole addresses.
**Milestone:** M2

#### Scenario: Spoofed From rejected
- **GIVEN** `authserv_id = "mx.google.com"`
- **WHEN** a mail claiming an allowlisted From address fails DMARC in its Authentication-Results header
- **THEN** the system SHALL drop it as unauthorized

#### Scenario: Threaded reply
- **WHEN** the agent replies to a message with Message-ID `<a@x>`
- **THEN** the reply SHALL carry `In-Reply-To: <a@x>` and a subject with exactly one `Re:` prefix

### Requirement: Matrix end-to-end encryption
`gateway.platforms.matrix.options.e2ee` SHALL accept `off`, `optional` (default) and `required`; with encryption the adapter SHALL keep Olm/Megolm device and session keys in a crypto store under the owning profile (`<profile>/platforms/matrix/crypto/`, mode 0700), upload device keys at first connect, and when the secret `MATRIX_RECOVERY_KEY` exists import cross-signing keys from secret storage and self-sign the device on every start; `required` SHALL fail closed (state `fatal`, code `matrix_e2ee_unavailable`) when encryption cannot initialize, a store whose one-time keys no longer match the server SHALL refuse encryption with remediation text, and an event that cannot be decrypted SHALL be dropped with one WARN per room and never passed to the agent as ciphertext.
**Milestone:** M3

#### Scenario: Required encryption fails closed
- **GIVEN** `e2ee = "required"` and a corrupted crypto store
- **WHEN** the adapter connects
- **THEN** it SHALL enter `fatal` with `matrix_e2ee_unavailable` and read no rooms

### Requirement: Adapter host forwarding mode
`prometheus daemon run --adapters-only --upstream <ws-url>` SHALL run only the configured adapters and forward every admitted inbound event to an upstream daemon over the client protocol using a ticket secret named by `daemon.upstream_ticket_secret`, delivering the upstream's replies through the local adapters, so platform-specific crypto or native dependencies can run on one host (for example Matrix encryption on a macOS host) while sessions run in another (for example a container); sessions SHALL be owned only by the upstream and an unreachable upstream SHALL put the forwarding host's adapters into `degraded`.
**Milestone:** M3

#### Scenario: Container agent with host crypto
- **GIVEN** a host running `--adapters-only` for Matrix with encryption and an upstream daemon in a container
- **WHEN** an encrypted Matrix message arrives
- **THEN** the host SHALL decrypt it, forward the event upstream, and send the upstream's reply encrypted

### Requirement: Matrix rooms, invites and clock skew
The Matrix adapter SHALL apply `options.allowed_rooms` (internal room ids, not aliases) before any other gating with two-member rooms treated as DMs and exempt, SHALL accept invites only from authorized users when `options.auto_join = "authorized"` (default; `all` and `none` also accepted), SHALL support `options.session_scope` in `auto` (default), `room` and `thread` where `room` isolates each project room into its own session and workspace binding, and SHALL drop live events older than its startup time minus 5 seconds, logging one WARN with the count when more than 10 are dropped later than 30 seconds after startup naming clock skew as the likely cause.
**Milestone:** M3

#### Scenario: Alias in allowlist rejected
- **WHEN** `allowed_rooms = ["#ops:example.org"]`
- **THEN** validation SHALL fail stating Matrix allowlists require `!`-prefixed room ids

### Requirement: Signal adapter specifics
The Signal adapter SHALL redact phone numbers in logs to the first 4 and last 4 characters (for example `+155****4567`), treat a sync message sent by the account to itself as inbound in Note-to-Self setups while suppressing echo of its own sends by sent timestamp, send Markdown as native text style ranges with native quoted replies, admit groups only listed in `options.allow_groups` (unset disables groups, `["*"]` allows all), refresh typing every 8 seconds, and monitor health by pinging `/api/v1/check` after 120 seconds of SSE inactivity with reconnect backoff from 2 to 60 seconds; when the daemon lacks text-style support it SHALL fall back to plain text with one WARN.
**Milestone:** M2

#### Scenario: Note to self
- **GIVEN** a single-number setup where the user messages their own account
- **WHEN** the user writes in Note to Self
- **THEN** the system SHALL admit it as inbound and SHALL NOT re-admit the agent's reply

### Requirement: WhatsApp pairing and modes
`prometheus gateway whatsapp pair` SHALL install the bridge runtime if needed, render the linking QR code in the terminal (refusing with exit 2 when the terminal is narrower than 60 columns and offering `--pairing-code`), refresh it until linked or 120 seconds elapse (exit 75), and store the linked-device session under `<profile>/platforms/whatsapp/session/` mode 0700; `options.mode` SHALL accept `bot` (a dedicated number) or `self_chat` (the user's own number, only the self chat admitted), re-running `pair` SHALL replace the session, transient disconnects SHALL reconnect without re-pairing, a logged-out session SHALL put the adapter in `fatal` with code `whatsapp_logged_out` and the remedy command, and each profile SHALL have its own session.
**Milestone:** M2

#### Scenario: Logged out from the phone
- **WHEN** the user removes the linked device in WhatsApp
- **THEN** the adapter SHALL enter `fatal` with `whatsapp_logged_out` naming `prometheus -p <profile> gateway whatsapp pair`

#### Scenario: Self-chat only
- **GIVEN** `mode = "self_chat"`
- **WHEN** a contact messages the user's number
- **THEN** the system SHALL not admit it

### Requirement: WhatsApp interactive and rich features
The WhatsApp adapter SHALL declare `interactive = "native"` by rendering clarify questions as native single- or multi-select polls whose votes are admitted as the answer (falling back to numbered text when a poll send fails) while approvals always use numbered text, SHALL send and receive locations (including live locations) as structured attachments, SHALL include quoted text and quoted media in inbound reply context resolving the bot's own recent sends from an index of the last 1000, SHALL apply group policy `options.group_policy` in `pairing` (default; no group admitted until approved), `allowlist` or `open`, and SHALL batch rapid messages with the host batching defaults.
**Milestone:** M2

#### Scenario: Clarify as poll
- **WHEN** the agent asks a clarify question with 3 choices on WhatsApp
- **THEN** the user SHALL receive a poll and their vote SHALL be returned to the agent as the answer

### Requirement: Clarify multi-select on platforms
The card model SHALL carry `multi_select` for clarify prompts: `native` adapters SHALL render multi-select controls (Discord select menus, Slack checkboxes, Google Chat and Teams card checkboxes, WhatsApp multi-select polls) with a submit action, `numbered` and `none` adapters SHALL append "Multiple selections allowed" and accept numbers separated by commas or spaces (for example `1, 3`), the option text, or free text as an "Other" answer, out-of-range numbers SHALL be answered with the valid range and keep the prompt pending, and an unanswered prompt SHALL expire after `agent.clarify_timeout_secs` (owned by `agent-turn-loop`).
**Milestone:** M2

#### Scenario: Numbered multi-answer
- **GIVEN** a multi-select clarify with 4 options on Signal
- **WHEN** the user replies "1, 3"
- **THEN** the agent SHALL receive options 1 and 3 as the answer

#### Scenario: Out of range
- **WHEN** the user replies "7" to a 4-option prompt
- **THEN** the system SHALL reply that valid choices are 1-4 and the prompt SHALL stay pending

### Requirement: Container media path translation
When the session's terminal runs in a container backend, the host SHALL translate a container-absolute path in an outbound attachment to its host path by longest-prefix match over the backend's declared volume mounts and the session workspace mount, then apply the normal attachment path checks to the translated path; a path matching no mount, or one under the container's Prometheus home that is not a mounted cache directory, SHALL NOT be translated and SHALL be withheld with the standard note.
**Milestone:** M2

#### Scenario: Chart rendered in container
- **GIVEN** the container mounts the session workspace at `/workspace`
- **WHEN** a reply references `/workspace/out/chart.png`
- **THEN** the system SHALL deliver the host file `<workspace>/out/chart.png` as an image

### Requirement: Deliverable mode
When `gateway.delivery.detect_paths = true` (default), the host SHALL extract from a final reply every bare absolute or `~/` path ending in a deliverable extension and existing on disk, ignoring paths inside fenced or inline code, remove each extracted path from the visible text, and deliver it by class: images (`png jpg jpeg gif webp bmp tiff svg`) inline, video (`mp4 mov avi mkv webm 3gp`) inline where `media_out` includes `video`, audio (`mp3 wav ogg opus m4a flac`) as voice or audio, and documents, data, geospatial, presentations, archives and web pages (`pdf docx doc odt rtf txt md epub xlsx xls ods csv tsv json xml yaml yml kmz kml geojson gpx pptx ppt odp key zip tar gz tgz bz2 xz 7z rar html htm`) as file uploads; source code and log extensions SHALL never be auto-delivered, and every detected path SHALL pass the attachment path checks.
**Milestone:** M2

#### Scenario: Report path becomes an upload
- **GIVEN** the agent's final reply is "Done: ~/project/report.pdf"
- **WHEN** it is delivered to Telegram
- **THEN** the chat SHALL receive "Done:" and `report.pdf` as a document

#### Scenario: Code block untouched
- **WHEN** a reply contains `/tmp/a.png` inside a fenced code block
- **THEN** the system SHALL deliver the text unchanged and attach nothing

### Requirement: Response-wide delivery modifiers and kanban artifacts
The modifier `[[as_document]]` anywhere in a reply, including text produced by a skill or skill script, SHALL make every attachment extracted from that reply deliver as a document (all-or-nothing per reply) and SHALL be removed from the delivered text, with `[[as_voice]]` applying the same scope to audio; a platform without document upload SHALL fall back to its closest supported kind; kanban task completion notifications SHALL upload each artifact path recorded on the completed task natively to subscribed chats, skipping missing files and noting the skipped names.
**Milestone:** M2

#### Scenario: Image sent uncompressed
- **WHEN** a skill's output contains `[[as_document]] MEDIA:/ws/photo.jpg`
- **THEN** the photo SHALL be delivered as a document and the text SHALL not contain `[[as_document]]`

#### Scenario: Kanban artifact delivered
- **GIVEN** a completed kanban task with artifact `report.xlsx`
- **WHEN** the completion notification goes to a subscribed Slack channel
- **THEN** the channel SHALL receive `report.xlsx` as a file upload

### Requirement: Webhook routes
The `webhook` adapter SHALL serve each route at `/p/<profile>/webhook/<route>` on the shared listener, routes being declared as `[gateway.platforms.webhook.routes.<name>]` with keys `events` (list; empty accepts all), `secret_secret` (required unless `insecure_no_auth = true`, which is accepted only while `gateway.http.listen` is loopback), `prompt` (template with `{a.b.c}` dot paths, unresolved paths kept literally, nested values rendered as JSON truncated to 2000 characters, `{__raw__}` the whole payload truncated to 4000; absent means the payload JSON truncated to 4000), `skills`, `toolsets` (replacing the default webhook toolset rather than merging), `deliver` (default `log`) and `deliver_extra`; the event type SHALL be taken from `X-GitHub-Event`, `X-GitLab-Event` or the payload's `event_type`, and signatures SHALL be verified per scheme (GitHub `X-Hub-Signature-256`, GitLab `X-Gitlab-Token` constant-time match, Standard Webhooks `webhook-id`/`webhook-timestamp`/`webhook-signature`, and generic `X-Webhook-Signature-V2` over `<ts>.<body>` within ±300 seconds), rejecting a request with no recognised signature header with 401.
**Milestone:** M2

#### Scenario: Event filtered by type
- **GIVEN** a route with `events = ["pull_request"]`
- **WHEN** GitHub posts a signed `push` event
- **THEN** the system SHALL respond 200 `{"status":"ignored","event":"push"}` and start no turn

#### Scenario: Prompt rendering
- **GIVEN** `prompt = "Review PR #{pull_request.number}: {pull_request.title}"`
- **WHEN** a signed pull_request payload with number 42 and title "Fix x" arrives
- **THEN** the turn's input SHALL be "Review PR #42: Fix x"

### Requirement: Webhook limits, idempotency and responses
The webhook adapter SHALL reject bodies over `gateway.platforms.webhook.max_body_bytes` (default 1048576) with 413 before reading them, apply a per-route fixed-window limit of `rate_limit_per_min` (default 30) answering 429, deduplicate deliveries for 1 hour by `X-GitHub-Delivery`, `svix-id`, `webhook-id` or `X-Request-ID` answering 200 `{"status":"duplicate"}`, answer 400 for invalid JSON and 404 for unknown routes, and acknowledge an accepted agent run with 202 `{"status":"accepted","route","event","delivery_id"}` before the turn completes.
**Milestone:** M2

#### Scenario: Redelivery deduplicated
- **WHEN** GitHub redelivers a payload with the same `X-GitHub-Delivery` within an hour
- **THEN** the system SHALL answer 200 duplicate and start no second turn

### Requirement: Webhook payload filters and transform scripts
A webhook route SHALL accept declarative `filters` evaluated after authentication and event matching, with operators `exists`, `missing`, `equals`, `not_equals`, `contains`, `in`, `in_file`, `regex` and groups `all`, `any`, `not` over paths `payload.<x>`, `event` and `headers.<Name>`, answering a non-match with 200 `{"status":"ignored","reason":"filter","route":<name>}`; and an optional `script` (a path under the profile's `scripts/`, traversal rejected) run in the sandbox with the payload JSON on stdin within `script_timeout_secs` (default 30, minimum 1), whose JSON-object stdout replaces the payload, other output is added as `script_output`, and whose empty output, `[SILENT]`, `{"ignore": true}`, timeout, missing file or non-zero exit answers 200 `{"status":"ignored","reason":"script"}`.
**Milestone:** M2

#### Scenario: Only failing CI runs
- **GIVEN** a filter `{equals = {path = "payload.workflow_run.conclusion", value = "failure"}}`
- **WHEN** a successful workflow_run event arrives
- **THEN** the system SHALL answer 200 with reason `filter` and start no turn

#### Scenario: Script escapes scripts directory
- **WHEN** a route declares `script = "../../bin/sh"`
- **THEN** validation SHALL fail naming the path traversal

### Requirement: Webhook event coalescing
A webhook route MAY declare `coalesce = {key, window_secs = 30, max_wait_secs = 300}` where `key` is a dot path or template; events resolving to the same key SHALL be held, each newer event replacing the pending one and restarting the quiet window, and one turn SHALL run when the window elapses or `max_wait_secs` after the first event, its input noting how many events were superseded; coalesced requests SHALL be answered 202 `{"status":"coalesced"}` after idempotency checks, an unresolvable key SHALL dispatch immediately, pending groups SHALL be flushed on adapter stop, and combining `coalesce` with `deliver_only` or `cron_job` SHALL fail validation.
**Milestone:** M3

#### Scenario: Burst of pushes
- **GIVEN** `coalesce.key = "payload.ref"` and `window_secs = 30`
- **WHEN** 5 pushes to the same branch arrive 5 seconds apart
- **THEN** the system SHALL run one turn on the last payload noting 4 superseded events

### Requirement: Webhook direct delivery mode
A webhook route with `deliver_only = true` SHALL run no model: the rendered prompt SHALL be sent verbatim to the `deliver` target, answering 200 `{"status":"delivered","route","target","delivery_id"}` on success and 502 `{"error":"Delivery failed"}` on delivery failure; `deliver_only` with `deliver = "log"` SHALL fail validation and the route's `skills` SHALL be ignored.
**Milestone:** M2

#### Scenario: Zero-LLM alert forwarding
- **GIVEN** a route with `deliver_only = true` and `deliver = "telegram"`
- **WHEN** a signed alert payload arrives
- **THEN** the rendered text SHALL be posted to the Telegram home channel without any model call

### Requirement: MCP server events into the webhook pipeline
The webhook adapter SHALL accept event routes created for MCP servers that publish events, signed with the Standard Webhooks scheme, answering control envelopes without a turn (`verification` echoes `{"challenge"}` of at most 512 characters else 400, `terminated` posts a delivery-only notice and answers `{"status":"terminated"}`, `gap` resumes from a fresh cursor and answers `{"status":"gap"}`, unknown types answer `{"status":"ignored","type"}`), and passing event occurrences through the normal route pipeline with idempotency key `mcp-event:<eventId>` and the event `data` (up to 32000 characters) substituted for `{data}` after template rendering so it is never interpreted as a placeholder.
**Milestone:** M3

#### Scenario: Placeholder injection inert
- **WHEN** an MCP event's data contains the text `{__raw__}`
- **THEN** the turn input SHALL contain that text literally and not the raw payload

### Requirement: Supervised sidecar adapters
An adapter plugin MAY declare a sidecar in `plugin.toml` (a managed runtime such as Node with a hash-pinned package set) that the daemon installs under `<home>/runtimes/`, starts on adapter connect bound to a loopback ephemeral port with a per-start bearer token, restarts with the adapter backoff, and stops on disconnect; the sidecar SHALL receive only the secrets the adapter declared and was granted, never other gateway or provider secrets, its port and token SHALL be published in `<home>/run/` for out-of-daemon senders, and a sidecar exit SHALL put the adapter in `retrying` with code `sidecar_exited`.
**Milestone:** M3

#### Scenario: Sidecar crash contained
- **WHEN** the Photon sidecar process exits unexpectedly
- **THEN** the Photon adapter SHALL enter `retrying` with `sidecar_exited` and restart it while other adapters keep running

### Requirement: Catalog adapter set
The signed plugin catalog SHALL provide first-party adapter plugins `whatsapp_cloud`, `msgraph_webhook`, `teams`, `google_chat`, `mattermost`, `dingtalk`, `feishu`, `wecom`, `wecom_callback`, `weixin`, `bluebubbles`, `photon`, `qqbot`, `yuanbao`, `line`, `sms`, `ntfy`, `irc`, `raft`, `buzz`, `simplex` and `homeassistant`, each passing the adapter conformance kit and declaring at least these descriptor limits: `teams` 28000 chars, `google_chat` 4000 chars, `mattermost` 16383 chars, `dingtalk` 20000 chars, `feishu` 4000 chars, `wecom_callback` 2048 utf8_bytes, `weixin` 4000 chars, `line` 5000 chars, `sms` 1600 chars, `ntfy` 4096 chars, `irc` 450 utf8_bytes, `whatsapp_cloud` 4096 chars; `prometheus migrate from-hermes` SHALL install the catalog plugin for every such platform enabled in the Hermes profile, disabled and without grants as `hermes-migration` › MCP servers and plugins requires.
**Milestone:** M3

#### Scenario: Migration installs adapter
- **GIVEN** a Hermes profile with Mattermost enabled
- **WHEN** it is migrated
- **THEN** the `mattermost` catalog adapter plugin SHALL be installed and `[gateway.platforms.mattermost]` populated with secret references

### Requirement: WhatsApp Cloud API adapter
The `whatsapp_cloud` adapter SHALL bind HTTP routes on the shared listener, answer the subscription handshake by echoing `hub.challenge` only when `hub.verify_token` equals the secret named by `options.verify_token_secret`, verify `X-Hub-Signature-256` as an HMAC-SHA256 of the raw body with the app secret (401 on mismatch, 503 when the app secret is missing), send via the Graph API version `options.api_version` (default `v20.0`) for phone number id `options.phone_number_id` (validated as 15-17 digits), render clarify with 1-3 choices as reply buttons and 4-10 as a list (labels truncated to 20 and row titles to 24 characters) and approvals as Approve/Deny buttons, mark inbound read with a typing indicator, convert speech to OGG/Opus voice notes, and on Graph error 131047 (outside the 24-hour window) send the template named by `options.reengagement_template` when configured and otherwise report the failure to the user's other surfaces as "WhatsApp 24-hour window closed"; the setup command `prometheus gateway whatsapp-cloud setup` SHALL generate the verify token.
**Milestone:** M3

#### Scenario: Wrong verify token
- **WHEN** a handshake GET carries a `hub.verify_token` that does not match
- **THEN** the system SHALL answer 403 and not echo the challenge

#### Scenario: Window closed
- **GIVEN** no `reengagement_template`
- **WHEN** a cron delivery to a user fails with 131047
- **THEN** the outbox entry SHALL be marked failed with "WhatsApp 24-hour window closed"

### Requirement: Microsoft Graph change-notification listener
The `msgraph_webhook` adapter SHALL bind `/p/<profile>/msgraph_webhook/` on the shared listener, echo `validationToken` as `text/plain` 200 within 10 seconds, require the secret named by `options.client_state_secret` (refusing to start without it) and compare each notification's `clientState` in constant time, drop items for resources outside `options.accepted_resources` (trailing `*` prefix match; empty accepts all), deduplicate the last `options.max_seen_receipts` (default 5000) notifications, answer 202 on success, 403 when every item fails `clientState` or the source address is outside `options.allowed_source_cidrs`, and 400 for malformed bodies; when `gateway.http.public = true` an empty `allowed_source_cidrs` SHALL fail validation.
**Milestone:** M3

#### Scenario: Validation handshake
- **WHEN** Graph sends `?validationToken=abc`
- **THEN** the system SHALL answer 200 `text/plain` body `abc`

### Requirement: Microsoft Teams adapter
The `teams` adapter SHALL receive Bot Framework activities on the shared listener and validate each request's Bot Framework JWT (issuer, audience = the configured app id, signing keys from the published metadata) before admission, cap request bodies at 1 MiB, reply in personal chats always and in group chats and channels only when @mentioned or replied to (stripping the mention) with `options.require_mention` applying to RSC-delivered channel messages, render approvals and clarify as Adaptive Cards (Allow Once, Allow Session, Always Allow, Deny) replaced in place with the decision, send typing activities, and identify users by AAD object id; secrets `TEAMS_CLIENT_SECRET` and options `client_id`, `tenant_id` are required.
**Milestone:** M3

#### Scenario: Unsigned activity
- **WHEN** an activity arrives without a valid Bot Framework JWT
- **THEN** the system SHALL answer 401 and admit nothing

### Requirement: Teams meeting summary pipeline
The signed catalog SHALL provide a `teams_meetings` plugin, built on the `msgraph_webhook` adapter, that MUST create and renew Microsoft Graph subscriptions for online-meeting transcript and recording resources (renewing before expiry and recording each subscription in profile state), fetch the transcript (falling back to the recording only when `options.use_recordings = true`) when a notification arrives, run one summary turn per meeting with the profile's main route, and deliver the summary by `options.delivery` = `incoming_webhook` (POST to the URL in the secret named by `options.webhook_secret`) or `graph` (post into the meeting chat); each meeting SHALL be summarized at most once (keyed by meeting id and transcript id), and the plugin SHALL provide `prometheus plugins run teams_meetings subscribe|list|renew|remove|status|replay <meeting-id>`.
**Milestone:** M3

#### Scenario: Duplicate notification summarized once
- **GIVEN** a meeting whose transcript was already summarized
- **WHEN** Graph delivers a second notification for the same transcript id
- **THEN** the plugin SHALL deliver no second summary and `status` SHALL show the notification as `duplicate`

#### Scenario: Subscription renewed before expiry
- **GIVEN** a transcript subscription expiring in 30 minutes
- **WHEN** the renewal ticker runs
- **THEN** the plugin SHALL extend the subscription and record the new expiry

### Requirement: Google Chat adapter
The `google_chat` adapter SHALL receive events either by Pub/Sub pull (`options.project_id`, `options.subscription`, service-account secret; default `max_messages` 1) or by HTTP callbacks on the shared listener verified by a Google-signed ID token whose audience and service-account email match configuration (401 otherwise), keep one session per thread, show a working-state message ("Prometheus is thinking…") edited in place with the reply unless `typing_indicator = false`, render clarify as a Card v2 with one button per choice plus "Other", send attachment bearer tokens only to Google hosts, and support `/setup-files start|<redirect-url>|revoke` to store a per-user OAuth token (scope `chat.messages.create`) in the secrets store for native file delivery, posting a text notice when the user has none.
**Milestone:** M3

#### Scenario: Per-user file delivery
- **GIVEN** a user who completed `/setup-files`
- **WHEN** the agent sends that user a PDF
- **THEN** the system SHALL upload it natively with that user's token

### Requirement: Mattermost adapter
The `mattermost` adapter SHALL use REST API v4 for sends and the `/api/v4/websocket` event stream for inbound with reconnect backoff from 2 to 60 seconds, reply in DMs always and in channels only on @mention unless the channel is in `free_response_channels`, apply `options.allowed_channels` before every other gate (non-listed channels dropped even when mentioned), support `options.reply_mode` in `off` (default) or `thread`, honour `/sethome`, and identify users by 26-character user ids with an empty allowlist denying all.
**Milestone:** M3

#### Scenario: Channel outside allowlist
- **GIVEN** `allowed_channels = ["town-square-id"]`
- **WHEN** a user @mentions the bot in another channel
- **THEN** the system SHALL ignore the message

### Requirement: DingTalk adapter
The `dingtalk` adapter SHALL receive messages over DingTalk Stream Mode (a long-lived outbound WebSocket needing no public URL), reply through each message's session webhook (reporting "No session_webhook available" when expired), stream replies as AI Cards when `options.card_template_id` is set, mark processing with a Thinking reaction replaced by Done, require @mention in groups unless `free_response_chats` or `mention_patterns` match, and truncate at 20000 characters.
**Milestone:** M3

#### Scenario: AI card streaming
- **GIVEN** `card_template_id` configured
- **WHEN** a reply streams
- **THEN** the system SHALL update one AI card progressively instead of sending multiple messages

### Requirement: Feishu and Lark adapter
The `feishu` adapter SHALL support `options.connection_mode` `websocket` (default) or `webhook` (verified by the encrypt key signature over timestamp, nonce, key and body and by the verification token, 401 on mismatch, rate limited to 120 requests per 60 seconds per source with 429, bodies capped at 1 MB, and a WARN after 25 consecutive errors from one source within 6 hours), `options.domain` `feishu` or `lark`, QR scan-to-create app setup in `prometheus gateway setup feishu`, card button callbacks (deduplicated for 15 minutes) for approvals and confirmations, Markdown sent as rich `post` messages with plain-text fallback, per-chat serialized processing with text batching (0.6 s, at most 8 messages or 4000 characters) and media batching (0.8 s), and group ACLs `options.group_policy` (`allowlist` default, `open`, `disabled`) with per-chat `group_rules.<chat_id> = {policy in open|allowlist|blacklist|admin_only|disabled, allowlist, blacklist, require_mention}`.
**Milestone:** M3

#### Scenario: Empty group allowlist
- **GIVEN** `group_policy = "allowlist"` with no entries
- **WHEN** a group message arrives
- **THEN** the system SHALL drop it and log one WARN for that chat

### Requirement: Feishu document comments and meeting invitations
The `feishu` adapter SHALL admit document comment events (when the app has drive comment permissions) as turns whose context includes the document text and the comment thread (last 20 messages for whole-document threads, 12 for selections), reply in the comment thread in chunks of 4000 characters, and authorize commenters by three tiers — exact document rule, then wildcard rule, then top-level rule, first match wins — each with policy `allowlist` or `pairing`, stored in profile state and managed by `prometheus gateway feishu comment-rules status|check|pairing`; meeting invitation events carrying an inviter and meeting number SHALL be admitted as turns subject to normal authorization and ignored otherwise.
**Milestone:** M3

#### Scenario: Unauthorized commenter
- **GIVEN** a document rule with policy `allowlist` not containing the commenter
- **WHEN** the commenter mentions the bot in a comment
- **THEN** the system SHALL start no turn

### Requirement: WeCom smart robot adapter
The `wecom` adapter SHALL connect to the WeCom AI Bot WebSocket (`options.websocket_url`, default `wss://openws.work.weixin.qq.com`) with bot id and secret, stream replies with the native stream message type (keep-alive every `stream_keepalive_secs`, default 120, switching to a proactive send after 330 seconds), decrypt inbound media with its per-message AES-256-CBC key, upload outbound media in 512 KB chunks within limits image 10 MB, voice 2 MB, video 10 MB, file 20 MB (oversized media downgraded to file, over 20 MB refused with a notice), and apply `options.dm_policy` (`open` default, `allowlist`, `pairing`, `disabled`), `options.group_policy` and per-group sender allowlists `groups.<id>.allow_from`.
**Milestone:** M3

#### Scenario: Per-group sender allowlist
- **GIVEN** `groups.G1.allow_from = ["alice"]`
- **WHEN** bob writes in group G1
- **THEN** the system SHALL not admit the message

### Requirement: WeCom callback adapter
The `wecom_callback` adapter SHALL serve encrypted XML callbacks on the shared listener, verify the SHA1 signature with the token and decrypt with the 43-character EncodingAESKey (WXBizMsgCrypt-compatible), answer the URL-verification GET, acknowledge each POST immediately and send the reply proactively through the app's message API, support multiple self-built apps in `options.apps` with users scoped as `<corp_id>:<user_id>` and replies routed with that app's token, and split text at 2048 bytes.
**Milestone:** M3

#### Scenario: Multi-app routing
- **GIVEN** two apps in different corps
- **WHEN** a user of app B writes
- **THEN** the reply SHALL be sent with app B's access token

### Requirement: Weixin personal WeChat adapter
The `weixin` adapter SHALL long-poll the iLink bot API (35-second timeout) persisting its sync cursor in profile state, log in by QR in `prometheus gateway setup weixin` storing the account token in the secrets store, persist and echo the per-peer `context_token` (retrying once without it on a stale-token error), decrypt and encrypt CDN media with AES-128-ECB, pause 10 minutes on session expiry, default `dm_policy = "open"` and `group_policy = "disabled"` (warning when enabled), and hold the machine-global credential lock for its token.
**Milestone:** M3

#### Scenario: Context token survives restart
- **GIVEN** a peer whose context token was stored
- **WHEN** the daemon restarts and the agent sends a cron message to that peer
- **THEN** the send SHALL carry the stored context token

### Requirement: BlueBubbles iMessage adapter
The `bluebubbles` adapter SHALL send through the BlueBubbles server REST API and register its own webhook for `new-message` and `updated-message` events on connect (pointing at its shared-listener route, removed on clean shutdown), address chats by email or phone resolved to chat GUIDs, strip Markdown, and use the server's Private API when available for tapback reactions, typing indicators and read receipts (`options.send_read_receipts`, default `true`); in group chats it SHALL respond only when `options.mention_patterns` match when `require_mention = true`, and DMs SHALL never be mention-gated.
**Milestone:** M3

#### Scenario: Webhook registered
- **WHEN** the adapter connects
- **THEN** it SHALL register a webhook on the BlueBubbles server for new and updated messages pointing at its route

### Requirement: Photon iMessage adapter
The `photon` adapter SHALL run as a sidecar adapter holding the Spectrum streaming connection, read inbound messages from the sidecar's loopback NDJSON stream with deduplication, send text, images, voice, video, documents, native polls and effects through sidecar endpoints, represent inbound attachments as metadata markers only (filename and MIME type, no bytes), send read receipts unless `options.read_receipts = false`, and provide `prometheus gateway photon setup --phone <E164>` (device login, project and secret provisioning into the secrets store, phone registration, sidecar install) and `prometheus gateway photon status`.
**Milestone:** M3

#### Scenario: Inbound attachment metadata
- **WHEN** a user sends a photo over Photon
- **THEN** the agent SHALL receive a marker naming the file and MIME type and no file bytes

### Requirement: QQ Bot adapter
The `qqbot` adapter SHALL receive events over the official QQ Bot API v2 WebSocket gateway and reply over REST, covering C2C, group @-mention, guild and DM chats, apply `options.dm_policy` and `options.group_policy` (`open`, `allowlist`, `disabled`) with `allow_from` and `group_allow_from`, render approvals with native message keyboards where the account permits and numbered text otherwise, upload large media in chunks, and transcribe voice by using QQ's provided recognition text when present and the profile's STT provider otherwise.
**Milestone:** M3

#### Scenario: Voice with platform transcript
- **WHEN** a QQ voice message arrives with platform recognition text
- **THEN** the system SHALL use that text without calling the STT provider

### Requirement: Yuanbao adapter
The `yuanbao` adapter SHALL connect to the Yuanbao WebSocket gateway with an HMAC-signed token derived from the app id and secret (connect timeout 15 s, heartbeat 30 s, reconnect backoff 1-60 s), use chat ids `direct:<account>` and `group:<group_code>`, send a running-status heartbeat every 2 seconds during a turn, upload media to the platform object store after validating source URLs against private-address targets, decode forwarded chat-history bundles including nested forwards, render stickers, and apply `dm_policy` and `group_policy` (`open` default, `allowlist`, `disabled`).
**Milestone:** M3

#### Scenario: Forwarded history decoded
- **WHEN** a user forwards a bundle of 5 chat messages
- **THEN** the agent SHALL receive the 5 messages' senders and texts in order

### Requirement: LINE adapter
The `line` adapter SHALL verify `X-Line-Signature` as HMAC-SHA256 with the channel secret on its shared-listener route, reply with the free single-use reply token while it is valid and fall back to the Push API afterwards, and when a turn exceeds `options.slow_response_secs` (default 45; 0 disables) spend the reply token on a template message "Still thinking" with a "Get answer" button whose postback delivers the cached answer with a fresh reply token; media SHALL be served from tokenized HTTPS URLs under `options.public_url` because LINE accepts no binary uploads, replies SHALL be chunked at about 4500 characters with at most 5 bubbles per send, and `allowed_users`, `allowed_groups` and `allowed_rooms` SHALL accept `U`, `C` and `R` ids respectively.
**Milestone:** M3

#### Scenario: Slow answer retrieved
- **GIVEN** a turn that takes 90 seconds
- **WHEN** 45 seconds pass
- **THEN** the user SHALL receive the "Still thinking" button and pressing it after completion SHALL deliver the answer without a Push charge

### Requirement: SMS adapter
The `sms` adapter SHALL receive Twilio webhooks on its shared-listener route, verify `X-Twilio-Signature` (HMAC-SHA1 over `options.public_url` and the sorted form parameters) rejecting mismatches with 401, send plain text with Markdown stripped split at 1600 characters on newlines then spaces, ignore messages from its own number, redact phone numbers in logs, deny unknown senders by default, and render interactive prompts in the `numbered` tier; `options.public_url` SHALL be required.
**Milestone:** M3

#### Scenario: Bad Twilio signature
- **WHEN** an inbound SMS POST carries an invalid signature
- **THEN** the system SHALL answer 401 and admit nothing

### Requirement: ntfy adapter
The `ntfy` adapter SHALL subscribe to `options.topic` on `options.server_url` (default `https://ntfy.sh`) as an HTTP stream with 55-second keep-alive and reconnect backoff 2, 5, 10, 30, 60 seconds reset after 60 seconds of uptime, publish replies to `options.publish_topic` (default the subscribe topic) with optional Markdown, truncate bodies over 4096 characters with a notice, treat the topic name as the only sender identity (publisher-supplied titles never used for authorization), and enter `fatal` with `ntfy_unauthorized` on 401 or `ntfy_topic_not_found` on 404; an adapter with no allowlist SHALL run outgoing-only, admitting nothing and serving as a delivery target for cron and `prometheus gateway send`.
**Milestone:** M3

#### Scenario: Outgoing-only target
- **GIVEN** ntfy configured with no allowlist
- **WHEN** a message is published to the topic by anyone
- **THEN** the system SHALL admit nothing while cron deliveries to `ntfy` still publish

### Requirement: IRC adapter
The `irc` adapter SHALL connect to `options.server` on `options.port` (default 6697 with `use_tls = true`, 6667 without), register `options.nickname` (default `prometheus-bot`) with an optional server password and NickServ IDENTIFY from secrets, treat each joined channel in `options.channels` as a group and private messages as DMs, send plain text split into lines within 450 bytes, support no media, threads, reactions or typing, and hold the credential lock keyed by server and nickname.
**Milestone:** M3

#### Scenario: Long reply split
- **WHEN** a 1200-byte reply is sent to an IRC channel
- **THEN** it SHALL be sent as several PRIVMSG lines each within 450 bytes

### Requirement: Raft wake-only adapter
The `raft` adapter SHALL be a wake-only adapter: it SHALL start the Raft CLI bridge as a sandboxed child with a per-start token on an ephemeral loopback port and only the Raft profile name in its environment, accept wake hints posted with that token, reject any wake payload carrying message content fields (`text`, `body`, `content`, `messages`), and inject a content-free wake notice into the session so the agent reads and replies with the Raft CLI; it SHALL hold no Raft credentials or delivery cursors.
**Milestone:** M3

#### Scenario: Content-bearing wake rejected
- **WHEN** a wake POST includes a `text` field
- **THEN** the adapter SHALL reject it and inject nothing

### Requirement: Buzz Nostr adapter
The `buzz` adapter SHALL receive community messages over a NIP-42-authenticated relay WebSocket (falling back to polling every `options.poll_interval_secs`, default 4, when `transport = "auto"` cannot connect) and send through the Buzz CLI with the private key passed by environment never argv, answer in channels only when mentioned by name, npub or hex key and in DMs always, suppress its own events by public key, seed a per-channel high-water mark at connect so history is not replayed, thread replies by NIP-10 by default, reconnect a socket idle for 5 minutes, and download inbound media only after authorization when it is HTTPS from the relay origin or `options.attachment_hosts` with a declared size and matching SHA-256.
**Milestone:** M3

#### Scenario: Media hash mismatch
- **WHEN** an inbound attachment's downloaded SHA-256 differs from the declared one
- **THEN** the system SHALL discard it and tell the agent the attachment failed verification

### Requirement: SimpleX adapter
The `simplex` adapter SHALL connect to a local `simplex-chat` daemon WebSocket at `options.ws_url`, identify contacts by opaque numeric contact ids (display names rejected in allowlists), auto-accept contact requests when `options.auto_accept = true` (default) while still denying unpaired contacts, ignore groups unless listed in `options.allow_groups`, receive files through the daemon's file-transfer events, and send images, voice, video and documents by file path.
**Milestone:** M3

#### Scenario: Display name in allowlist
- **WHEN** `allow_users = ["alice"]`
- **THEN** validation SHALL fail stating SimpleX allowlists take numeric contact ids

### Requirement: Home Assistant adapter and toolset
The `homeassistant` catalog plugin SHALL provide an event adapter subscribed to `state_changed` over the Home Assistant WebSocket API at `options.url` (default `http://homeassistant.local:8123`) authenticated with the secret `HASS_TOKEN` (heartbeat 30 s, reconnect 5-60 s) that forwards only entities matching `options.watch_domains`, `options.watch_entities` or `options.watch_all` (default `false`) after `options.ignore_entities`, with a per-entity cooldown `options.cooldown_secs` (default 30) and a WARN when no filter is set, delivers replies as persistent notifications and accepts the cron delivery target `homeassistant:<notify target>`, and a toolset `homeassistant` with `ha_list_entities(domain?, area?)`, `ha_get_state(entity_id)`, `ha_list_services(domain?)` and `ha_call_service(domain, service, entity_id?, data?)`, where `entity_id` SHALL match `^[a-z_][a-z0-9_]*\.[a-z0-9_]+$` and calls to domains `shell_command`, `command_line`, `python_script`, `pyscript`, `hassio` and `rest_command` SHALL be refused.
**Milestone:** M3

#### Scenario: Dangerous domain blocked
- **WHEN** the agent calls `ha_call_service(domain = "shell_command", service = "run")`
- **THEN** the tool SHALL refuse with an error naming the blocked domain

### Requirement: Platform-contributed toolsets
An adapter MAY contribute a toolset whose tools are typed tool descriptors available only in sessions whose origin platform is that adapter or when the adapter is configured and the user enabled the toolset, resolved once at session start as part of the frozen tool surface; actions requiring platform permissions the bot lacks (detected at connect and cached for 24 hours) SHALL be hidden from the schema, a failed detection SHALL expose all actions, and every call SHALL be re-checked against the adapter's action allowlist at call time.
**Milestone:** M3

#### Scenario: Toolset absent elsewhere
- **GIVEN** the Feishu adapter is not configured
- **WHEN** a CLI session starts
- **THEN** no `feishu_*` tool SHALL be in its tool surface

### Requirement: Discord tools
The Discord adapter SHALL contribute tool `discord` with actions `fetch_messages(channel_id, limit = 50)`, `search_members(guild_id, query)` and `create_thread(channel_id, name, auto_archive_duration = 1440)`, and tool `discord_admin` with actions `list_guilds`, `server_info`, `list_channels`, `channel_info`, `list_roles`, `member_info`, `list_pins`, `pin_message`, `unpin_message`, `delete_message`, `add_role` and `remove_role`, all `effect = "external"`; `gateway.platforms.discord.options.server_actions` SHALL be an allowlist filtering both schemas and enforced at call time with "action <name> disabled by config", unknown action names SHALL fail validation, and without the Server Members intent `member_info` and `search_members` SHALL be hidden.
**Milestone:** M3

#### Scenario: Action not allowlisted
- **GIVEN** `server_actions = ["fetch_messages", "list_channels"]`
- **WHEN** the model calls `discord_admin` with action `add_role`
- **THEN** the tool SHALL fail with "action add_role disabled by config"

### Requirement: Feishu and Yuanbao tools
The Feishu adapter SHALL contribute toolset `feishu_doc` with `feishu_doc_read(doc_token)` returning plain text and toolset `feishu_drive` with `feishu_drive_list_comments(file_token, file_type = "docx", is_whole = false, page_size = 100, page_token?)`, `feishu_drive_list_comment_replies(file_token, comment_id, file_type, page_size ≤ 100, page_token?)`, `feishu_drive_reply_comment(file_token, comment_id, content, file_type)` and `feishu_drive_add_comment(file_token, content, file_type)`; the Yuanbao adapter SHALL contribute toolset `yuanbao` with `yb_query_group_info(group_code)`, `yb_query_group_members(group_code, action in find|list_bots|list_all, name?)`, `yb_send_dm(name, text, media?)` resolving the recipient among group members, `yb_search_sticker(query, limit = 10, max 50)` and `yb_send_sticker(sticker, ref_msg_id?)`.
**Milestone:** M3

#### Scenario: Sticker search bounded
- **WHEN** the model calls `yb_search_sticker` with `limit = 200`
- **THEN** the tool SHALL fail argument validation naming the maximum of 50

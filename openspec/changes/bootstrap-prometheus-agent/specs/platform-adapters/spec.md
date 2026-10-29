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
The capability descriptor SHALL declare `platform`, `max_message_length` (positive integer, required), `length_unit` in `chars`, `utf16`, `utf8_bytes`, `splits_long_messages`, `supports_edit`, `supports_delete`, `supports_typing`, `supports_reactions`, `supports_threads`, `interactive` in `native`, `numbered`, `none`, `media_in` and `media_out` (sets of `image`, `audio`, `voice`, `video`, `document`), `delivery_receipts`, `read_receipts`, `streaming_tts`, `markup` in `plain`, `markdown`, `html`, `platform`, and `binds_http`; the gateway SHALL read limits and capabilities only from the descriptor, a descriptor missing a required field or with a non-positive `max_message_length` SHALL fail adapter load, and `prometheus gateway platforms` SHALL print each loaded adapter's descriptor.
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
When splitting a reply would exceed `gateway.delivery.max_parts` parts (default 10), the host SHALL send the first part followed by a final part ending "... [truncated, full output attached]" and SHALL attach the full text as a `document` when the descriptor's `media_out` includes `document`, otherwise store it in the blob store and append "full output saved as <blob id>; run `prometheus session show <session_id>`"; adapters with `splits_long_messages = true` SHALL receive the full text.
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

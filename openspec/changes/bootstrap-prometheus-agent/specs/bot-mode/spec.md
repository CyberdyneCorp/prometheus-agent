# Bot Mode

## Purpose
Bot Mode presents profiles as a roster of named bots — each with a persistent canonical Bot Chat, title, avatar, voice, routines and memory — that can message each other with `message_agent` and deliberate together in group chats (rooms) driven durably by the daemon, including rooms whose members live on other gateways. The key decisions are: a bot is exactly a profile (no new storage primitive; everything is reachable from the CLI), bot metadata lives on the backend so every client sees the same bot, the bot protocol is frozen into a Bot Chat's prompt at session start and roster changes arrive as appended notices so the prompt cache is never broken, and every bot-to-bot delivery is durable, typed and never executed twice.

## ADDED Requirements

### Requirement: A bot is a profile with backend metadata
Every profile with `bot_mode.enabled = true` (default true) SHALL appear as a bot whose metadata — `title`, `description`, `avatar`, `hidden`, `section {id, name}` and group memberships — is stored in that profile's `state.db` and read and written only through `bot.meta.get` and `bot.meta.update {profile, changes}`, so every client attached to the same daemon shows the same bot; a bot's config, secrets, skills, memory and sessions SHALL be the profile's own, and `prometheus -p <bot> chat` SHALL open the same agent the roster opens.
**Milestone:** M3

#### Scenario: Title follows the bot to a second desktop
- **GIVEN** desktop A sets bot `research`'s title to "Research Buddy"
- **WHEN** desktop B attached to the same daemon lists the roster
- **THEN** it SHALL show "Research Buddy" for `research`

### Requirement: Canonical Bot Chat
Each bot SHALL have exactly one canonical Bot Chat obtained through `bot.chat.ensure` (the desktop-app get-or-create transaction) or `prometheus -p <bot> chat --bot-chat` (which uses the same transaction headlessly), marked with the session flag `bot_chat = true`; inside it `/new` and `/reset` SHALL run `/compress` instead of starting a new session, the idle auto-archive SHALL never archive it, and an explicit archive SHALL retire it so the next ensure creates a new canonical Bot Chat while the retired one stays stored and hidden; no title, file or UI marker other than the session flag SHALL be required for bot protocol features, so a headless install has the same Bot Chat behaviour as the desktop.
**Milestone:** M3

#### Scenario: /new keeps the relationship
- **GIVEN** a canonical Bot Chat with 300 messages
- **WHEN** the user types `/new`
- **THEN** the system SHALL compress the session in place and the bot SHALL still have one canonical Bot Chat

#### Scenario: Headless Bot Chat
- **GIVEN** a server with no desktop app
- **WHEN** the user runs `prometheus -p helper chat --bot-chat`
- **THEN** the session SHALL be helper's canonical Bot Chat and `message_agent` SHALL be available in it

### Requirement: Creating, duplicating and deleting bots
`bot.create {name, title, description, clone_from?, empty?, model?, soul?, skills?, toolsets?, mcp_servers?, copy_api_keys?, on?}` SHALL create a profile (fresh, cloned from `clone_from`'s config, skills, `SOUL.md` and memory, or with no bundled skills when `empty`), pin the model when given, copy static API-key secrets from the source when `copy_api_keys` (default true) but never copy OAuth or single-use logins, create its canonical Bot Chat and run one introduction turn as its first message; `on` naming a registered peer or desktop connection SHALL create the bot on that machine through its API (`POST /api/bots`), cloning only from that machine's profiles; `bot.duplicate {profile, name}` SHALL clone config, skills, `SOUL.md`, memory and metadata, and `bot.delete {profile, confirm: true}` SHALL delete the profile and refuse `default`.
**Milestone:** M3

#### Scenario: OAuth not copied
- **GIVEN** the source profile has an API key for `openrouter` and an OAuth login for `anthropic`
- **WHEN** a bot is created with `copy_api_keys = true`
- **THEN** the new profile SHALL have the `openrouter` key and SHALL NOT have the `anthropic` login

#### Scenario: Create on a remote machine
- **GIVEN** a registered peer `homelab`
- **WHEN** `bot.create` is called with `on = "homelab"`
- **THEN** the profile SHALL be created on homelab and appear in the roster as `@<name>-homelab` when the name also exists locally

### Requirement: Avatars
A bot's `avatar` SHALL be one of `blob` (default; a soft-body face derived deterministically from the bot name, with an optional locked seed and one of six silhouettes `round`, `organic`, `boxy`, `nub`, `cloud`, `sun`), `geometric` (one of 7 shapes × 10 colours), `image` (an uploaded image stored content-addressed in the profile's `blobs/`, at most 5 MiB, PNG, JPEG or WebP), `generated` (an image produced through the profile's image-generation backend and then stored as `image`) or `pet` (a pets-gallery companion id); the same name SHALL always yield the same blob face unless locked, and an unknown avatar kind SHALL be rejected by `bot.meta.update`.
**Milestone:** M3

#### Scenario: Deterministic default face
- **WHEN** two desktops render bot `scribe` with no avatar set
- **THEN** both SHALL draw the identical blob face

### Requirement: Roster presentation, hiding and sections
`bot.roster.list` SHALL return one row per bot with avatar, title, latest-message preview, timestamp, unread count and an `active` flag true when the bot owns a focused running turn, wrote within the last 90 seconds, or has a worker heartbeat; `hidden = true` SHALL remove a bot from the default roster and toasts without affecting @mentions, group memberships or routines (unread activity still accumulates), a section SHALL be stored as `{id, name}` on each member so another client rebuilds headings from members, and deleting a section SHALL move its bots to unassigned and never delete a bot.
**Milestone:** M3

#### Scenario: Hidden bot still reachable
- **GIVEN** bot `archivist` is hidden
- **WHEN** another bot calls `message_agent(target="archivist", ...)`
- **THEN** the message SHALL be delivered normally

### Requirement: Bot voices
Speech synthesis in a bot's chats (read aloud, auto-speak, voice conversation) SHALL use the bot's own profile `tts.*` settings on the daemon that owns the bot, so two bots with different voices sound different and same-named bots on different gateways never share a voice, while speech-to-text in a Bot Chat SHALL use the attaching client's active profile settings; a bot without its own `tts.*` SHALL use its profile's defaults.
**Milestone:** M3

#### Scenario: Two bots, two voices
- **GIVEN** bot `alice` with `tts.voice = "nova"` and bot `bob` with `tts.voice = "onyx"`
- **WHEN** the user reads aloud a message in each Bot Chat
- **THEN** alice's SHALL be synthesized with `nova` and bob's with `onyx`

### Requirement: Bot routines
A routine SHALL be a scheduled job in the bot's own profile named `[bot:<name>] <routine>` whose delivery target is `bot_chat`, appending each run's result to the bot's canonical Bot Chat as an attributed message; routines SHALL be listed, edited, paused and removed with the ordinary scheduled-jobs commands (`prometheus -p <bot> cron list` shows them), `bot.routines.list {profile}` SHALL return them for the routines pane, and the schedule picker SHALL produce only schedule strings that `prometheus cron` accepts.
**Milestone:** M3

#### Scenario: Morning digest lands in the Bot Chat
- **GIVEN** routine `[bot:inbox] morning digest` scheduled `0 8 * * *`
- **WHEN** it runs at 08:00
- **THEN** its result SHALL appear in inbox's canonical Bot Chat and the run SHALL be in `prometheus -p inbox cron runs`

### Requirement: Bot protocol in canonical Bot Chats
When `bot_mode.protocol = true` (default true) the system SHALL add a bot-messaging protocol section — the bot's own handle and the teammate roster (local bots, peer agents and other-connection agents with handle, title, role and machine) — only to canonical Bot Chat sessions, resolved once when the session's system prompt is built and frozen for the session; roster changes (bots added, renamed or removed, peers registered) SHALL be delivered to an existing Bot Chat as one appended `roster.update` notice before its next turn, never by rebuilding the system prompt, and ordinary sessions and `SOUL.md` SHALL be unaffected.
**Milestone:** M3

#### Scenario: New teammate mid-session
- **GIVEN** an active Bot Chat whose prompt lists two teammates
- **WHEN** a third bot is created
- **THEN** the next turn SHALL carry a `roster.update` notice naming the new bot and the system prompt hash SHALL be unchanged

### Requirement: message_agent tool
Canonical Bot Chats SHALL have the `message_agent(target, message)` tool, resolving `target` in order: exact profile name, `@`-tag of a bot title (`Research Buddy` → `@research-buddy` or `@researchbuddy`), `@prometheus` for the primary bot, `<peer>/<agent>` or `<peer>` for a peer gateway, `<handle>@<connection>` for another desktop connection; a title shared by two bots SHALL be refused with the candidate list, and an unknown target SHALL be refused with the roster; the delivered text SHALL be prefixed "Message from 🤖 <title> (@<handle>)", delivery SHALL be a durable ingress record in the target profile that runs as the target Bot Chat's next turn (after any running turn and queued human prompts), returning `queued` on durable admission and the reply on `settled`; a delivery whose target stays busy longer than `bot_mode.turn_wait_secs` (default 120) SHALL fail with reason `target_busy`, a chain of bot messages SHALL be cut after `bot_mode.max_hops` (default 4) consecutive bot-originated hops with reason `hop_limit` (upgrade), and a target answering with an intentional silence token SHALL return an empty reply.
**Milestone:** M3

#### Scenario: Friendly name cannot hijack a profile name
- **GIVEN** profile `scribe` and another bot titled "Scribe" with profile `writer`
- **WHEN** a bot calls `message_agent(target="scribe", ...)`
- **THEN** the message SHALL go to profile `scribe`

#### Scenario: Busy target
- **GIVEN** the target bot runs a 10-minute turn
- **WHEN** a second delivery waits 120 seconds
- **THEN** it SHALL fail with reason `target_busy`

### Requirement: Typed delivery failures and safe retry
Every failed bot turn or delivery SHALL carry a `reason` from `provider_auth_or_access`, `provider_quota_limit`, `provider_rate_limit`, `provider_server_error`, `context_overflow`, `missing_config`, `model_unavailable`, `runtime_offline`, `queued_expired`, `delivery_timeout`, `target_busy`, `hop_limit` or `unknown` alongside redacted error text, shown to the sending agent as `[reason: <code>]`; a failed delivery turn SHALL be retried at most once and only for `runtime_offline`, `delivery_timeout`, `provider_rate_limit`, `provider_server_error` or `context_overflow` (compressing first), re-running the same Bot Chat session without appending the message a second time, while auth, quota and configuration failures SHALL never auto-retry; envelopes older than `bot_mode.envelope_ttl_secs` (default 900, 0 disabling) when drained SHALL fail with `queued_expired`.
**Milestone:** M3

#### Scenario: Quota failure surfaces at once
- **WHEN** the target's provider answers with a quota error
- **THEN** the sender SHALL receive `[reason: provider_quota_limit]` and no retry SHALL run

#### Scenario: Retry keeps one copy
- **GIVEN** a delivery failed with `provider_server_error`
- **WHEN** it is retried
- **THEN** the target Bot Chat transcript SHALL contain the incoming message exactly once

### Requirement: Mentions from any chat
When a user's message in any chat contains `@handle` that resolves against the live roster, the system SHALL attach to that turn a notice naming the resolved bot (profile, title and machine) so the active agent can contact it with `message_agent`, SHALL never forward the user's text verbatim to the mentioned bot, and SHALL leave unknown `@words` and e-mail addresses untouched.
**Milestone:** M3

#### Scenario: Mention is resolved, not forwarded
- **WHEN** the user writes "@researcher have a look at this" in a Bot Chat
- **THEN** the turn SHALL carry a notice identifying profile `researcher` and nothing SHALL be delivered to researcher unless the agent calls `message_agent`

### Requirement: Group chats
A group chat (room) SHALL have a stable room id, a unique display name, an optional picture and 2–6 member bots, each member keeping its own room session titled `Group: <room> · <thread>`; a user message SHALL drive up to `hosted_rooms.discussion.max_rounds` (default 3) serial rounds and at most `hosted_rooms.discussion.max_messages` (default 10) member messages per send, where @-mentioned members (or all members when none is mentioned) take turns, each member replies or passes, the room settles after a fully silent round, `@user` raises a needs-you flag, and each member turn receives the room messages since its last turn up to 200 messages or 32000 characters with any single message cut to an 8000-character marked excerpt and a count of omitted messages; renaming SHALL keep history and sessions, a name already taken SHALL be refused, and `groups.compress_member {room_id, member}` SHALL compress one member's room session.
**Milestone:** M3

#### Scenario: Rooms do not spin
- **GIVEN** a room of four bots that keep replying to each other
- **WHEN** the user sends one message
- **THEN** at most 10 member messages and 3 rounds SHALL follow before the room settles

#### Scenario: Silent round settles
- **WHEN** every member passes in round 1
- **THEN** the room SHALL settle without round 2

### Requirement: Room stop directives and held members
In a room, `stop`, `halt` or `pause` SHALL hold a member only when the word is adjacent to that member's @mention (outside code spans, fenced code, quotes and blockquotes), a held member SHALL take no turns until it is @mentioned again or the whole room is addressed with `@all` or `@everyone`, and on release it SHALL receive every message its hold skipped; the room setting `detect_stop_directives` (default true) MAY disable directive detection, while the Stop control SHALL always cancel the active run and queued continuations.
**Milestone:** M3

#### Scenario: Stop word in prose
- **WHEN** the user writes "@bot the halt condition is x > 3"
- **THEN** the bot SHALL NOT be held

#### Scenario: Release delivers skipped messages
- **GIVEN** `@builder stop` held builder while three messages were posted
- **WHEN** the user writes "@all continue"
- **THEN** builder's next turn SHALL include the three skipped messages

### Requirement: Durable room driver
When every member of a room lives on one daemon, that daemon SHALL drive the room under a driver lease stored in `state.db` (one driver per room even when several processes share the home), so closing or disconnecting every client never stops a discussion and clients catch up from the room log; a member turn SHALL be kept while it reports progress for up to 10800 seconds and expire after 180 seconds without progress, the room log SHALL record `turn.started` and `turn.settled` with outcome and typed failure reason, a member failure SHALL appear in the room's activity summary as "<member> hit an error — <first line, redacted>", and an ambiguous submit failure SHALL NOT be resubmitted automatically; `groups.capabilities` SHALL report `driver: true` when this applies.
**Milestone:** M3

#### Scenario: Client closed mid-discussion
- **GIVEN** a room discussion in round 2
- **WHEN** the only desktop quits
- **THEN** the daemon SHALL finish the rounds and the next client SHALL see all replies in the room log

### Requirement: Hosted rooms across gateways
A room whose members live on several gateways SHALL be hosted by one authority gateway that owns the room log (sequence numbers and `authority_epoch`) and drives turns, reaching remote members through their API server with a per-room grant (scopes `post`, `stop`, `approve`, issued by the member gateway's owner and revocable) under which a remote member's turn runs as an authority run in the member's own room session on its own machine; a member turn that outlives the authority's connection SHALL finish on the member's machine and be posted at the next drive, never re-driven while still running; non-authority gateways SHALL keep a replica through `groups.log` and `groups.replicate` pages with `groups.replica_state {room_id}` reporting `last_seq` and `latest_seq`, and a send to a non-authority SHALL be refused with "room authority is <gateway>".
**Milestone:** M3

#### Scenario: Remote member without a grant
- **GIVEN** a member gateway that has not issued a room grant
- **WHEN** the authority tries to drive that member
- **THEN** the member gateway SHALL answer 403 and the room SHALL show the member as unreachable with reason `missing_config`

### Requirement: Room authority transfer
`groups.promote {room_id, confirm: true, reason?}` on a replacement gateway SHALL make it the room authority with `authority_epoch` one greater than the replicated epoch and report `authority_gateway_id` and `authority_epoch`, failing with application error `confirmation_required` when `confirm` is absent and with `replica_incomplete` when `last_seq < latest_seq` unless `force_incomplete: true` is given (upgrade); `groups.demote {room_id, observed_gateway_id, observed_epoch}` (all required) on the old authority SHALL record `authority.lost`, adopt the newer lineage idempotently and refuse further room writes; neither call SHALL merge diverged histories, and the operator documentation generated for these methods SHALL state that the old writer must be fenced before promotion.
**Milestone:** M3

#### Scenario: Promotion without confirmation
- **WHEN** `groups.promote {"room_id":"r1"}` is called
- **THEN** the system SHALL fail with `confirmation_required` and change nothing

#### Scenario: Demoted authority refuses sends
- **GIVEN** the old authority was demoted with the new gateway's id and epoch
- **WHEN** a client posts to the room on the old authority
- **THEN** the system SHALL refuse with "room authority is <new gateway>"

### Requirement: Bot Mode off switch and CLI parity
Setting `bot_mode.enabled = false` for a profile SHALL remove it from rosters, stop adding the bot protocol and `message_agent` to its new sessions and refuse incoming bot deliveries with reason `missing_config`, while leaving its profile, sessions, routines and room history untouched; every Bot Mode operation SHALL have a terminal equivalent: chatting (`prometheus -p <bot> chat --bot-chat`), files (`<home>/profiles/<bot>/`), routines (`prometheus -p <bot> cron list`), roster, metadata, groups and room authority (`prometheus rpc bot.*` and `prometheus rpc groups.*`), and peers (`prometheus gateway peer`).
**Milestone:** M3

#### Scenario: Disabled bot not messageable
- **GIVEN** `bot_mode.enabled = false` in profile `quiet`
- **WHEN** another bot calls `message_agent(target="quiet", ...)`
- **THEN** the call SHALL fail with `[reason: missing_config]` and quiet's sessions SHALL be unchanged

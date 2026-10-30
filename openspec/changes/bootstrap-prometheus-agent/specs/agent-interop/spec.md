# Agent Interop

## Purpose
Agent interop covers every way a Prometheus agent talks to agents and chats outside its own daemon: the open Agent2Agent (A2A) protocol in both directions (an inbound A2A server and the outbound `a2a_*` tools), peer gateways on other machines reached over their API server (`prometheus gateway peer`), the desktop courier that relays bot-to-bot messages between connected machines, the union roster of agents across gateways, and one-shot cross-platform delivery (`prometheus gateway send` and the `send_message` tool). The key decisions are: every crossing is authenticated per peer and fail-closed, peer input is always framed as untrusted, conversation identity is an exact (peer, tenant, context) binding rather than a title match, and credentials for another machine never leave the secrets store of the machine that holds them.

## ADDED Requirements

### Requirement: A2A server enablement and binding
The daemon SHALL serve the A2A protocol (v1.0) for a profile only when that profile sets `a2a.server.enabled = true` (default false), on the daemon-wide listener `a2a.listen` in `<home>/daemon.toml` (default `127.0.0.1:9900`) at `/p/<profile>/` and, for `default`, also at `/`; a non-loopback `a2a.listen` (IPv4, or IPv6 literal bare or bracketed) SHALL be refused unless `a2a.public = true` and every served profile has at least one peer token configured, and with no token configured the server SHALL answer only requests addressed to a loopback name or the configured `a2a.server.public_url` host and SHALL refuse browser-originated requests (an `Origin` header, a `Sec-Fetch-Site` of `cross-site` or `same-site`, or a non-JSON `Content-Type`) with 403.
**Milestone:** M3

#### Scenario: Wildcard bind without tokens
- **WHEN** `a2a.listen = "0.0.0.0:9900"` and profile `default` has no peer token
- **THEN** the system SHALL fail validation with "non-loopback a2a.listen requires a2a.public = true and peer tokens"

#### Scenario: Browser page refused
- **GIVEN** no token and a loopback bind
- **WHEN** a web page POSTs to `http://127.0.0.1:9900/` with an `Origin` header
- **THEN** the system SHALL answer 403 "browser-originated request refused"

### Requirement: Agent Card
The A2A server SHALL serve the Agent Card at `GET /.well-known/agent-card.json` (and the legacy `/.well-known/agent.json`) containing the agent name (`a2a.server.agent_name`, default `<hostname>-<profile>`), description, protocol version, the callable URL (`a2a.server.public_url`, else the bound address; `X-Forwarded-Host` and `X-Forwarded-Proto` honoured only when `a2a.server.trust_forwarded_headers = true`, default false), streaming and push-notification support, the authentication schemes required, and one skill per toolset in `a2a.server.advertised_toolsets` (default all toolsets enabled for A2A sessions), each with id, name and description, and SHALL never list a toolset that inbound A2A sessions cannot use.
**Milestone:** M3

#### Scenario: Card behind a reverse proxy
- **GIVEN** `public_url = "https://agent.example.com/a2a"`
- **WHEN** a peer fetches the card
- **THEN** the card's URL SHALL be `https://agent.example.com/a2a` rather than the bind address

### Requirement: A2A JSON-RPC methods and task lifecycle
The A2A server SHALL accept JSON-RPC 2.0 at `POST /` with methods `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask` and push-notification configuration create/get/list/delete, plus the pre-1.0 aliases `message/send`, `message/stream`, `tasks/get`, `tasks/cancel`, `tasks/resubscribe` and `tasks/pushNotificationConfig/*`; tasks SHALL move through `submitted`, `working`, `input-required`, then `completed`, `canceled`, `failed` or `rejected`, streaming SHALL send JSON-RPC-enveloped SSE frames with status and artifact updates, the final agent reply SHALL be the task's artifact, a task SHALL wait up to `a2a.server.reply_timeout_secs` (default 300, minimum 300) for the reply and the orphan sweep SHALL never fail a task before that window or while a request is still waiting on it, and an unknown method SHALL answer `-32601`.
**Milestone:** M3

#### Scenario: Interoperates with the official SDK
- **WHEN** a client built on the official A2A Python SDK resolves the card and calls `SendMessage` with a text part
- **THEN** the system SHALL return a task in `completed` state whose artifact contains the agent's reply

#### Scenario: Long task without timeout loss
- **GIVEN** a task whose turn takes 400 seconds with `reply_timeout_secs = 600`
- **WHEN** the caller polls `GetTask`
- **THEN** the task SHALL report `working` until the reply arrives and then `completed`

### Requirement: A2A conversation identity
Each inbound A2A task SHALL run in a live gateway session of the profile keyed `<profile>:a2a:<peer_identity>[@<tenant>]:<context_id>` (each component percent-encoded), so a peer can hold a multi-turn exchange by reusing its `contextId` and two peers or two contexts never share a transcript; the binding SHALL be stored with the session, a context id SHALL never be matched by session title, and an imported Hermes A2A session whose binding cannot be proven SHALL stay read-only and be reported by `prometheus doctor` rather than attached.
**Milestone:** M3

#### Scenario: Same context id from two peers
- **GIVEN** peers `alice` and `bob` both send `contextId = "ctx-1"`
- **WHEN** both tasks run
- **THEN** they SHALL run in two different sessions

### Requirement: A2A authentication, trust and rate limits
The A2A server SHALL authenticate callers by bearer token: per-peer tokens declared as `[[a2a.server.peers]]` with `name` and `token_secret` (the authenticated name driving trust, rate limits and audit), or a shared `a2a.server.shared_token_secret` whose identity falls back to the caller IP; it SHALL admit only identities in `a2a.server.trusted_peers` unless `a2a.server.allow_all_peers = true` (default false, WARN on every start), SHALL limit each identity to `a2a.server.rate_limit_per_minute` (default 60) answering excess with a JSON-RPC error and HTTP 429, and SHALL compare tokens in constant time; a bad token SHALL answer 401.
**Milestone:** M3

#### Scenario: Untrusted identity
- **GIVEN** peer `carol` has a valid token but is not in `trusted_peers`
- **WHEN** carol sends `SendMessage`
- **THEN** the system SHALL answer with task state `rejected` and start no turn

### Requirement: A2A inbound safety
Inbound A2A text SHALL be scanned by the prompt-injection scanner and presented to the model framed as untrusted input from the named peer, SHALL never be interpreted as a slash command, and SHALL run with the toolsets in `a2a.server.toolsets` (default the `api-server` toolset) with approvals resolved as unattended deny; outbound replies and artifacts SHALL pass forced secret redaction; each `contextId` SHALL be capped at `a2a.server.max_pingpong_turns` (default 5, maximum 20) exchanges within 10 minutes, further messages being `rejected` with reason `pingpong_cap`; and every exchange (peer, context, task id, direction, sizes, outcome — not content unless `a2a.server.audit_content = true`) SHALL be appended to the profile's audit log.
**Milestone:** M3

#### Scenario: Slash command from a peer
- **WHEN** a peer sends the text `/model gpt-5`
- **THEN** the agent SHALL receive it as ordinary untrusted text and the session model SHALL NOT change

#### Scenario: Ping-pong capped
- **GIVEN** two agents exchanged 5 messages in context `c1` within 10 minutes
- **WHEN** a sixth message arrives in `c1`
- **THEN** the task SHALL be `rejected` with reason `pingpong_cap`

### Requirement: A2A push notifications
For a task with a registered push-notification configuration the server SHALL POST task status updates to the configured URL signed with HMAC-SHA256 over the raw body using the secret named by `a2a.server.push_secret` (default the authenticating peer's token) in header `X-A2A-Signature: sha256=<hex>` with a timestamp header covered by the signature, SHALL subject push URLs to the outbound egress policy (private, loopback and link-local addresses refused unless allowlisted), and SHALL retry a failed push 3 times with exponential backoff from 2 seconds before marking it failed in `GetTask`.
**Milestone:** M3

#### Scenario: Push to a private address
- **WHEN** a peer registers push URL `http://169.254.169.254/hook`
- **THEN** the system SHALL refuse the configuration with an egress-policy error

### Requirement: Outbound A2A tools
The `a2a` toolset (disabled by default, enabled per surface like any toolset and usable from CLI, TUI, gateway and scheduled sessions) SHALL provide `a2a_discover(url)` (fetch and summarize an Agent Card), `a2a_call(agent, message, context_id?)` (send a task to a configured agent name or URL and return the reply; multi-turn via `context_id`), `a2a_list()` (configured agents, saved conversations and call metrics), `a2a_history(context_id)` (a persisted conversation) and `a2a_orchestrate(capability, message, mode)` (fan out to every configured agent advertising the capability, at most 8, with `mode` `all` returning every reply, `first` the first success, `best` the reply an auxiliary-model judge ranks highest with all replies attached); agents SHALL be configured as `[a2a.agents.<name>]` with `url`, optional `auth_secret`, `timeout_secs` (default 120) and `capabilities`, conversations SHALL be persisted in `state.db`, the tools SHALL declare `effect = "external"` and reach a host only under a capability grant for it, and peer replies SHALL be returned as untrusted tool output.
**Milestone:** M3

#### Scenario: Call a configured researcher
- **GIVEN** `[a2a.agents.researcher]` with a granted host
- **WHEN** the model calls `a2a_call("researcher", "summarize today's arXiv postings")`
- **THEN** the tool SHALL return the peer's reply and `a2a_history` SHALL show the exchange under the returned `context_id`

#### Scenario: Unknown agent name
- **WHEN** the model calls `a2a_call("ghost", "hi")` and `ghost` is neither configured nor a URL
- **THEN** the tool SHALL return an error listing the configured agent names

### Requirement: Peer gateway registry
`prometheus gateway peer add <name> --url <api-server-base-url> [--note <text>]` SHALL register a peer gateway whose name matches `^[a-z][a-z0-9-]{0,31}$`, reading the peer's API server key from stdin (never from argv) into the secrets store as `PEER_<NAME>_KEY`, writing `[peers.<name>]` (`url`, `note`, `key_secret`) to the profile's `config.toml`, and probing `GET /v1/capabilities` on the peer (an unreachable peer is a warning, a 401 is exit 78); `prometheus gateway peer list [--json]` SHALL print each peer with URL, note and last probe result, and `prometheus gateway peer remove <name>` SHALL delete both the entry and its secret; registering or removing a peer SHALL affect bot-chat rosters only from their next session or roster-update notice, never by rewriting a live system prompt.
**Milestone:** M3

#### Scenario: Key never on the command line
- **WHEN** the user runs `prometheus gateway peer add spark --url http://spark.lan:8642` and pastes the key on stdin
- **THEN** the key SHALL be stored only as secret `PEER_SPARK_KEY` and SHALL NOT appear in `config.toml`, shell history or logs

#### Scenario: Wrong key
- **WHEN** the probe answers 401
- **THEN** the command SHALL exit 78 with "peer spark rejected the key" and register nothing

### Requirement: Peer direct messages and remote runs
`prometheus gateway peer dm <peer>[/<agent>] [message]` (message from the argument or, when absent, stdin read verbatim) SHALL ensure the remote agent's canonical Bot Chat through the peer's `POST /api/bot-chat/ensure` (profile `<agent>` on a multiplexed peer, the peer's default otherwise), run one turn there over the session-chat endpoint, and print the reply on stdout; when the peer accepted the message but the turn outlives the connection (or still runs after 300 seconds) it SHALL print "message delivered to <peer>'s Bot Chat; turn still running (run <id>)" rather than "unreachable", so resending never runs the turn twice; `peer run <peer>[/<agent>] [message] [--idempotency-key <key>]` SHALL submit an asynchronous run in that Bot Chat and print its `run_id`, `peer status <peer> <run_id>` and `peer stop <peer> <run_id>` SHALL query and stop exactly that run, every subcommand SHALL accept `--json`, and exit codes SHALL be 0 on success, 1 when the peer rejects the request, 2 on usage errors, 66 for an unknown peer or agent, 69 when the peer is unreachable while connecting and 78 when its key is missing or rejected.
**Milestone:** M3

#### Scenario: Idempotent remote run
- **GIVEN** `prometheus gateway peer run spark --idempotency-key ticket-123` returned `run_A` and the network then dropped
- **WHEN** the same command is retried
- **THEN** it SHALL print `run_A` and the peer SHALL run the task once

#### Scenario: Peer offline
- **WHEN** `prometheus gateway peer dm spark "status?"` cannot connect
- **THEN** the command SHALL exit 69 with "peer spark unreachable"

### Requirement: Union roster across gateways
The daemon SHALL expose `GET /api/agents` on the API server (each served profile marked as a bot, with name, title, description, avatar reference, model and whether it accepts bot messages) and `bot.roster.list {include_remote}` over RPC, the latter merging local profiles, registered peers' `/api/agents` (refreshed every 300 seconds and on demand) and agents reported by attached desktop clients for their other connections; when the same name exists on several sources each SHALL get the handle `@<name>-<device>` (device = peer name or connection label) while unique names keep `@<name>`, an unreachable source SHALL keep its last-known rows marked `stale` with the error instead of removing them, and a remote `default` profile SHALL be listed under its bot title (or `<hostname>@<source>` when untitled) rather than as a second `@prometheus`.
**Milestone:** M3

#### Scenario: Same name on two machines
- **GIVEN** a local profile `research` and peer `homelab` also serving `research`
- **WHEN** the roster is listed
- **THEN** the entries SHALL be `@research-local` and `@research-homelab`

#### Scenario: Peer temporarily down
- **GIVEN** peer `spark` listed two agents and is now unreachable
- **WHEN** the roster refreshes
- **THEN** both rows SHALL remain with `stale = true` and the connection error

### Requirement: Desktop relay for cross-connection bot messages
When a bot message targets an agent on another desktop connection that no peer entry reaches, the sending daemon SHALL queue a signed envelope (sender, target handle, message, delivery id, created time) in its outbox, an attached desktop that holds both connections SHALL drain it with `bot.relay.drain` and hand it to the target daemon with `bot.relay.deliver` over that connection's own socket (gateways never receiving each other's credentials), the target SHALL run the message as its canonical Bot Chat's next turn bounded by `bot_mode.relay_turn_timeout_secs` (default 600) and return the reply the same way; an envelope drained but not acknowledged SHALL be re-offered once after the courier's delivery deadline (1560 seconds), the sender SHALL wait at most twice that before reporting reason `delivery_timeout`, an envelope older than `bot_mode.envelope_ttl_secs` when drained SHALL be failed with reason `queued_expired`, and a delivery id SHALL never run twice on the target.
**Milestone:** M3

#### Scenario: Desktop closed mid-delivery
- **GIVEN** an envelope drained by the desktop which then quits before delivering
- **WHEN** 1560 seconds pass and another desktop drains
- **THEN** the envelope SHALL be offered once more and, if still undelivered after 3120 seconds total, the sender SHALL receive reason `delivery_timeout`

### Requirement: One-shot send command
`prometheus gateway send --to <target> [message] [--file <path>|-] [--subject <line>] [-q] [--json]` SHALL deliver one message without starting an agent turn, where `<target>` is `platform` (its home channel), `platform:chat_id`, `platform:chat_id:thread_id` or `platform:#channel-name`, the body comes from the argument, `--file` (text only) or non-TTY stdin, `MEDIA:<path>` directives attach files natively and `[[as_document]]` sends images uncompressed; `--list [platform]` SHALL print known targets (home channels, allowed chats, chats with sessions); when the daemon is running the message SHALL go through its outbox, and otherwise platforms whose adapter declares `stateless_send` (Telegram, Discord, Slack, Signal, WhatsApp Cloud, SMS-style plugins) SHALL send directly with the profile's credentials while others exit 69 "daemon required for <platform>"; exit codes SHALL be 0 sent, 1 delivery failure, 2 usage error, 66 unknown target and 78 platform not configured.
**Milestone:** M2

#### Scenario: Pipe a status line
- **WHEN** a script runs `echo "RAM 92%" | prometheus gateway send --to telegram:-1001234567890 -q`
- **THEN** the system SHALL send "RAM 92%" to that chat, print nothing and exit 0

#### Scenario: No home channel
- **GIVEN** Slack has no home channel
- **WHEN** the user runs `prometheus gateway send --to slack "hi"`
- **THEN** the command SHALL exit 66 with "no home channel for slack; run /sethome in the target chat"

### Requirement: send_message tool
The `send_message` tool (toolset `messaging`) SHALL accept `action` `send` (default), `list`, `react` or `unreact`, a `target` in the send-target grammar, `message` and optional `emoji`, and SHALL authorize every target before sending: the chat MUST be the platform's home channel, listed in `allow_chats`, or a chat in which the profile already has a session, and relay-fronted targets MUST pass the relay's egress authorization, where any error inside an authorization check SHALL refuse the send (fail closed); sends SHALL go through the outbox, a sent message SHALL be mirrored into the target chat's session as an attributed assistant message, `MEDIA:` paths SHALL be readable under the session's read grants, reactions SHALL require a live adapter that supports them, and the tool SHALL be excluded from subagents, API and A2A sessions and webhook runs unless their toolsets name `messaging` explicitly.
**Milestone:** M2

#### Scenario: Unknown chat refused
- **WHEN** the model calls `send_message` with target `telegram:999` that is neither home, allowed nor known
- **THEN** the tool SHALL return "target telegram:999 is not an authorized destination" and send nothing

#### Scenario: Guard fault fails closed
- **GIVEN** the relay egress check raises an internal error
- **WHEN** the model sends to a relay-fronted Slack channel
- **THEN** the tool SHALL refuse the send

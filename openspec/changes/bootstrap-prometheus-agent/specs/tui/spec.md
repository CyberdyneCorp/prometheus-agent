# TUI and Client Protocol

## Purpose
The TUI is the full-screen terminal interface compiled into the `prometheus` binary (no Node or Python runtime), and this capability also owns the **session-attachment semantics** of the client protocol (attachments, roles, presence, server-to-client requests, busy input) that every local client (TUI, desktop, dashboard, ACP, interactive CLI) uses on top of the wire contract owned by `workspace-architecture` (handshake `rpc.hello`, JSON-RPC error table, WebSocket close codes, `schema/rpc.json`). The key decision is that the TUI owns only the screen while the gateway daemon owns sessions, agents and approvals, so one session can be watched and driven from several clients at once; the wire schema is generated from typed definitions, and unknown methods, params, events and session access without an attachment are refused, never silently tolerated.

## ADDED Requirements

### Requirement: TUI launch selection
The system SHALL start the TUI for `prometheus tui`, and for bare `prometheus` or `prometheus chat` without `-q` only when both stdin and stdout are TTYs and `ui.interface` is `"tui"` (default `"tui"`; the other value is `"line"`); `--line` SHALL force the line-mode CLI and `--tui` SHALL force the TUI, and no environment variable SHALL influence the choice.
**Milestone:** M2

#### Scenario: Piped invocation never opens the TUI
- **GIVEN** `ui.interface = "tui"`
- **WHEN** `prometheus chat` runs with stdout piped to a file
- **THEN** the system SHALL use the line-mode CLI and SHALL NOT enter the alternate screen

#### Scenario: Forced TUI without a terminal
- **WHEN** the user runs `prometheus tui` with stdin not a TTY
- **THEN** the system SHALL print `prometheus tui needs an interactive terminal (stdin is not a TTY)` to stderr and exit 2

### Requirement: TUI attaches to the gateway daemon as a client
The TUI SHALL NOT run an agent loop in its own process by default; it SHALL ensure the gateway daemon of the active Prometheus home is running (starting it detached when `daemon.autostart = true` in `<home>/daemon.toml`, the default), obtain a one-use `interactive` ticket over the daemon's owner-only control socket, and connect over loopback WebSocket; when the daemon is absent and `daemon.autostart = false` it SHALL exit 69 with `gateway daemon is not running; start it with 'prometheus daemon start'`.
**Milestone:** M2

#### Scenario: First launch starts the daemon
- **GIVEN** no daemon is running for the home
- **WHEN** the user runs `prometheus tui`
- **THEN** the system SHALL start the daemon detached, wait at most 30 s for its ready file, and attach the TUI to it

#### Scenario: Autostart disabled
- **GIVEN** `daemon.autostart = false` and no daemon running
- **WHEN** the user runs `prometheus tui`
- **THEN** the system SHALL exit 69 with the daemon-not-running message and SHALL NOT open `state.db`

### Requirement: Standalone in-process mode
`prometheus tui --standalone` SHALL run an in-process gateway that takes the home's exclusive daemon lock for its lifetime and serves only this TUI; when another daemon already holds the lock it SHALL exit 75 with `<home> is served by a running gateway daemon (pid <pid>); run without --standalone to attach`.
**Milestone:** M2

#### Scenario: Standalone beside a live daemon
- **GIVEN** a daemon holds the home lock
- **WHEN** the user runs `prometheus tui --standalone`
- **THEN** the system SHALL exit 75 without opening `state.db` as a writer

### Requirement: Terminal state is always restored
The TUI SHALL restore the terminal (leave raw mode, leave the alternate screen, show the cursor, disable mouse capture and bracketed paste) on normal exit, on `SIGINT`/`SIGTERM`/`SIGHUP`, and on a panic, and SHALL write any panic report to `<home>/logs/tui-panic-<timestamp>.log` rather than to the restored screen only.
**Milestone:** M2

#### Scenario: Panic during render
- **WHEN** a render function panics
- **THEN** the terminal SHALL be back in cooked mode on the primary screen, the panic report path SHALL be printed to stderr, and the process SHALL exit 70

### Requirement: In-TUI update hands off without a relaunch protocol
The `/update` command in the TUI SHALL detach from the daemon, restore the terminal, and run the same code path as `prometheus update` in the same process, then exit with that command's exit code; no magic exit code SHALL be used to signal an update request.
**Milestone:** M2

#### Scenario: Update requested from the TUI
- **WHEN** the user runs `/update`
- **THEN** the TUI SHALL restore the terminal before the updater prints anything and the process exit code SHALL be the updater's

### Requirement: Client protocol framing
The TUI and every other client SHALL use the framing defined by `workspace-architecture`: JSON-RPC 2.0 with one JSON object per WebSocket text frame (and one per line on stdio transports); an unparseable frame SHALL be answered with `-32700 parse error` and `id: null` and the connection SHALL stay open; a non-object SHALL get `-32600 invalid request: expected an object`; frames larger than `daemon.rpc.max_frame_bytes` (default 16 MiB) SHALL close the connection with code 4413.
**Milestone:** M2

#### Scenario: Garbage frame
- **WHEN** a client sends the text frame `not json`
- **THEN** the daemon SHALL respond `-32700 parse error` with `id: null` and SHALL process the client's next frame normally

### Requirement: Hello handshake is mandatory and versioned
The first request on every connection SHALL be the `rpc.hello` handshake defined by `workspace-architecture`, whose `client` field SHALL be one of `tui`, `cli`, `desktop`, `dashboard`, `acp` (the connection's client kind); any other first method SHALL be refused with `-32008 handshake required before <method>`; a protocol major the daemon does not serve SHALL be refused with `-32008` naming both versions and `prometheus update`, then the connection SHALL close with 4426; the TUI SHALL read `profiles`, `epoch` and `platform_support` from the `rpc.hello` result.
**Milestone:** M2

#### Scenario: Old client against a new daemon
- **GIVEN** the daemon serves protocol major 2
- **WHEN** a client sends `rpc.hello` with protocol `1.4`
- **THEN** the reply SHALL be `-32008` naming both versions and `prometheus update`, and the connection SHALL close with 4426

#### Scenario: Skipping the handshake
- **WHEN** a client sends `session.create` first
- **THEN** the response SHALL be error `-32008` and no session SHALL be created

### Requirement: Typed method registry is enforced
Every method, server-to-client request and event SHALL be declared in one typed registry from which the checked-in schema `schema/rpc.json` and the generated TypeScript bindings are produced (as owned by `workspace-architecture`); the daemon SHALL answer an undeclared method with `-32601 unknown method: <method>`, a param key not declared by the method with `-32602 unknown param <path>; client and gateway are out of sync, run 'prometheus update'`, and a missing or mistyped required param with `-32602` naming the path; a CI check SHALL fail when the checked-in schema or bindings differ from a fresh generation.
**Milestone:** M2

#### Scenario: Misspelled param
- **WHEN** a client calls `prompt.submit` with an extra key `sesion_id`
- **THEN** the response SHALL be `-32602` whose message names `sesion_id` and `prometheus update`

#### Scenario: Handler without a declaration
- **WHEN** a developer adds a handler that is not in the registry
- **THEN** the build SHALL fail, because handlers can only be registered through a registry entry

### Requirement: Error code and close code taxonomy
The client protocol SHALL use only the JSON-RPC error codes of the `workspace-architecture` table, applied to session semantics as: `-32002` for a session not attached on the connection, a profile not granted, an observer attempting input, a source or author the connection may not claim; `-32003` not found; `-32004` conflict (invalid state, already-settled request); `-32006` daemon retiring; `-32008` handshake or protocol version error; `-32099` internal error; each with a distinguishing `data.code` (for example `session.not_attached`, `profile.not_granted`, `rpc.forged_author`). It SHALL use exactly the WebSocket close codes of that table on every client listener (TUI, desktop, dashboard, ACP bridge): 1000 normal, 1001 gateway shutting down, 1011 internal error, 4401 missing or invalid credential, 4403 Host, Origin, peer or profile not allowed, 4404 endpoint disabled on this surface, 4408 heartbeat timeout, 4410 session ended, 4413 frame too large, 4426 protocol version unsupported.
**Milestone:** M2

#### Scenario: Same refusal, same code everywhere
- **GIVEN** an upgrade request with a forbidden Origin
- **WHEN** it targets the TUI listener or the dashboard listener
- **THEN** both SHALL close with 4403

#### Scenario: Disabled endpoint
- **WHEN** a client upgrades to an endpoint its surface does not expose
- **THEN** the socket SHALL close with 4404 regardless of credential

### Requirement: Session access requires an attachment
Every session-addressed method (prompt, events, approvals, subagents, completion, mutation) SHALL require that the calling connection has attached to that session with `session.attach {session_id, role}` and that the session belongs to a profile the connection's ticket grants; otherwise the daemon SHALL answer `-32002` with `data.code` `session.not_attached` (`session <id> is not attached on this connection`) or `profile.not_granted` (`profile <name> is not granted to this connection`), including for `session.events.since`.
**Milestone:** M2

#### Scenario: Replaying a foreign session's events
- **GIVEN** a connection whose ticket grants profile `work`
- **WHEN** it calls `session.events.since` for a session of profile `personal`
- **THEN** the response SHALL be `-32002` and no event SHALL be returned

#### Scenario: Guessing a session id
- **WHEN** a connection calls `subagent.list` for a session it never attached
- **THEN** the response SHALL be `-32002` with `data.code` `session.not_attached`

### Requirement: Event sequencing and reconnect replay
Every event frame SHALL carry `session_id`, a per-session monotonic `seq` and the daemon `epoch`; the daemon SHALL retain events with the limits defined by `workspace-architecture` (per session the last 512 events or 4 MiB, whichever is smaller, at most 64 MiB in total, evicting oldest first); `session.attach` with `last_seen_seq` SHALL replay retained events after it and return `{latest_seq, epoch, truncated, open_requests}`, where `truncated` is true when events after `last_seen_seq` were evicted or the epoch changed, in which case the client SHALL reload the transcript with `session.transcript`.
**Milestone:** M2

#### Scenario: Reconnect after a daemon restart
- **GIVEN** a client holding epoch E1 and seq 900
- **WHEN** it re-attaches to a daemon whose epoch is E2
- **THEN** the response SHALL have `truncated: true` and the client SHALL reload the transcript instead of applying stale deltas

### Requirement: Transport liveness
The daemon SHALL answer `rpc.ping` with `{"ok": true}` from the connection's read loop without queueing it behind other requests; clients SHALL send a heartbeat ping every 15 s and treat 45 s without any frame as dead; the daemon SHALL close a connection silent for 45 s with 4408 and SHALL bound a single send to 30 s before closing with 1011.
**Milestone:** M2

#### Scenario: Ping during a long compaction
- **GIVEN** a `session.compact` request is running on the connection
- **WHEN** the client sends `rpc.ping`
- **THEN** the daemon SHALL reply `{"ok": true}` before the compaction finishes

### Requirement: Long requests do not block the connection
Requests declared `long` in the registry (for example `session.resume`, `session.list`, `session.compact`, `command.dispatch`, `complete.path`, `model.options`) SHALL run as independent tasks on the worker pool bounded by `daemon.rpc.workers` (default 8) and answer out of order, while all other requests SHALL be answered in arrival order; a long request that panics SHALL produce `-32099 internal error: <method>` and SHALL NOT close the connection.
**Milestone:** M2

#### Scenario: Slow resume
- **WHEN** a client calls `session.resume` then `commands.catalog`
- **THEN** the catalog response MAY arrive before the resume response and the resume SHALL still complete

### Requirement: Gateway retirement refuses new work
While the daemon is retiring (update, restart or shutdown), it SHALL refuse every new request except `rpc.ping` and `session.detach` with `-32006 daemon is restarting; reconnect to continue`, SHALL let running turns finish up to `daemon.restart_drain_secs` (default 120), and SHALL then close connections with 1001.
**Milestone:** M2

#### Scenario: Prompt during retirement
- **WHEN** a client sends `prompt.submit` during retirement
- **THEN** the response SHALL be `-32006` and no turn SHALL start

### Requirement: Server-to-client requests
The daemon SHALL ask clients questions (`approval.request`, `clarify.request`, `secret.request`, `sudo.request`) as JSON-RPC requests with ids `srq-<12 hex>` sent to every attached client whose `rpc.hello` advertised `capabilities.server_requests = true` and whose role is `driver`; the first valid answer SHALL settle the request and the daemon SHALL emit `request.resolved {id, by_client}` to all other recipients; when no attached client can answer, the request SHALL settle immediately with outcome `no_responder` instead of waiting out its deadline.
**Milestone:** M2

#### Scenario: Two windows show the same approval
- **GIVEN** a TUI and a desktop window both attached as drivers
- **WHEN** the TUI answers the approval
- **THEN** the desktop SHALL receive `request.resolved` naming the TUI client and SHALL withdraw its card

#### Scenario: No capable client attached
- **GIVEN** only a client that advertised `server_requests: false`
- **WHEN** the agent needs approval
- **THEN** the request SHALL settle at once with `no_responder`, which the approval policy SHALL treat as a denial

### Requirement: Server request deadlines fail closed
Every server-to-client request SHALL carry a `deadline_ms` (`approvals.timeout_secs`, default 300, for approvals; `daemon.rpc.request_timeout_secs`, default 600, for clarify and other requests); on expiry the daemon SHALL settle approvals as denied and clarify as unanswered, emit one `request.cancel {id, reason: "timeout"}`, and a client answer arriving after settlement SHALL receive `-32004 request <id> already settled`.
**Milestone:** M2

#### Scenario: Late answer
- **GIVEN** an approval that timed out and was denied
- **WHEN** a client answers it with `approval.respond {decision: "once"}`
- **THEN** the response SHALL be `-32004` and the tool call SHALL remain denied

### Requirement: Idempotent session creation
`session.create {profile, source, idempotency_key?, cwd?, model?}` SHALL return the same session for retries with the same `idempotency_key` within 300 s, SHALL require `source` to match the connection's `client_kind`, and SHALL reject an incoherent model and provider pair with `-32602` before any session row exists.
**Milestone:** M2

#### Scenario: Lost create response
- **WHEN** a client retries `session.create` with the same `idempotency_key` 10 s later
- **THEN** the response SHALL carry the original `session_id` and only one session row SHALL exist

#### Scenario: Source spoofing
- **GIVEN** a connection whose `rpc.hello` said `client: "dashboard"`
- **WHEN** it calls `session.create` with `source: "tui"`
- **THEN** the response SHALL be `-32002`

### Requirement: Prompt submission while busy
`prompt.submit {session_id, input_id, content}` arriving while a turn is running SHALL never be dropped; it SHALL follow `busy.input_mode` (`queue` default, `steer`, `interrupt`; per-platform overrides under `gateway.platforms.<platform>.busy` apply only to messaging sessions), where `steer` injects after the current atomic tool step and never hard-interrupts, `queue` runs the input FIFO after the turn, and `interrupt` cancels the turn at the next safe point and then runs the input; a repeated `input_id` SHALL return the original admission instead of a second turn.
**Milestone:** M2

#### Scenario: Duplicate submit after a lost ack
- **WHEN** a client resends `prompt.submit` with the same `input_id`
- **THEN** the response SHALL carry the original admission id and no second turn SHALL run

### Requirement: Turn authors are stamped by the gateway
The daemon SHALL stamp each admitted input with `{client_kind, client_id, principal}` from the connection, and `prompt.submit` carrying a client-supplied author SHALL be refused with `-32002` (`data.code` `rpc.forged_author`, `turn author is stamped by the gateway, never by a client`).
**Milestone:** M2

#### Scenario: Forged author
- **WHEN** a client sends `prompt.submit` with an `author` field
- **THEN** the response SHALL be `-32002` with `data.code` `rpc.forged_author` and no turn SHALL start

### Requirement: Profile scope on every request
Every request that reads or writes profile data SHALL execute under an explicit ProfileScope resolved from `params.profile`, else the attached session's profile, else the connection's single granted profile; when the connection is granted several profiles and none is named the daemon SHALL answer `-32602 profile is required: this connection is granted several profiles`.
**Milestone:** M2

#### Scenario: Config read for a second profile
- **GIVEN** a connection granted profiles `default` and `work`
- **WHEN** it calls `config.get {key: "model.default"}` without `profile`
- **THEN** the response SHALL be `-32602` naming the missing profile and no profile's config SHALL be read

### Requirement: Slash command flow in the TUI
The TUI SHALL handle client-local commands from its own typed registry (`/quit`, `/clear`, `/copy`, `/paste`, `/theme`, `/keys`, `/pane`, `/layout`) without a round-trip and send every other `/` input to `command.dispatch {session_id, name, args}`, which SHALL answer one of `{type: "done", notice}`, `{type: "send", message}` (submitted as a prompt) or `{type: "refused", reason}`; text whose first token is not a registered command (for example `/etc/hosts`) SHALL be sent as a prompt.
**Milestone:** M2

#### Scenario: Skill command
- **WHEN** the user types `/deploy staging` and `deploy` is an installed skill
- **THEN** `command.dispatch` SHALL return `{type: "send", message}` and the TUI SHALL submit that message as a prompt

#### Scenario: A path is not a command
- **WHEN** the user types `/var/log/syslog is huge`
- **THEN** the TUI SHALL submit the text as a prompt

### Requirement: Discovery RPCs preserve the prompt cache
Discovery and presentation methods (`commands.catalog`, `complete.slash`, `complete.path`, `model.options`, `session.transcript`) SHALL NOT rebuild the system prompt, change the toolset or reload the memory snapshot of a live session, and session-affecting changes requested from the TUI SHALL take effect from the next session unless invoked with `--now`.
**Milestone:** M2

#### Scenario: Listing commands mid-conversation
- **WHEN** the TUI calls `commands.catalog` during a conversation
- **THEN** the session's cached system prompt hash SHALL be unchanged

### Requirement: Composer and path completion
The TUI composer SHALL support multi-line input (`Alt+Enter` or `Shift+Enter` inserts a newline), per-profile history persisted in `<profile>/tui/history` (at most 1000 entries, secrets redacted), bracketed paste, `@path` mentions and slash completion; `complete.path` SHALL list entries through the session's own terminal backend (never the daemon host's filesystem for a non-local backend) and an unreachable backend SHALL yield an empty list with no error.
**Milestone:** M2

#### Scenario: Container backend completion
- **GIVEN** a session whose terminal backend is a container
- **WHEN** the user tab-completes `@./src/`
- **THEN** the entries SHALL come from the container's filesystem

### Requirement: Status line and accessibility
The TUI SHALL show a status line with profile, model, context-window use as a percentage, session cost in the ledger currency and the number of pending approvals; it SHALL honour `NO_COLOR` and `ui.tui.theme`, offer `ui.tui.reduced_motion` (default false) to disable spinners and animation, and remain usable at 40 columns by 12 rows.
**Milestone:** M2

#### Scenario: Narrow terminal
- **WHEN** the terminal is resized to 40x12
- **THEN** the TUI SHALL keep the composer and the last transcript line visible and SHALL NOT panic

### Requirement: Configurable key bindings
The TUI SHALL read key bindings from `ui.tui.keys` (a table of action name to key chord), SHALL reject at startup an unknown action name or an unparseable chord with a config validation error naming the key, and `/keys` SHALL list the effective bindings.
**Milestone:** M2

#### Scenario: Unknown action
- **WHEN** `ui.tui.keys` contains `scrool_up = "PageUp"`
- **THEN** the TUI SHALL refuse to start with an error naming `ui.tui.keys.scrool_up` and exit 78

### Requirement: Split panes for subagents and tool output (upgrade)
The TUI SHALL offer a subagents pane and a tool-output pane beside the transcript, toggled by `/pane subagents|tools|off` or `F2`/`F3`; panes SHALL lay out side by side at 120 columns or more, stacked between 80 and 119 columns, and as tabs below 80; the subagents pane SHALL list each child with status, elapsed time, tokens and cost and stream the selected child's transcript via `subagent.subscribe {session_id, subagent_id}` events rather than polling; the tool-output pane SHALL show the full streamed output of the selected tool call, while the transcript keeps the truncated summary.
**Milestone:** M3

#### Scenario: Watching a delegated child
- **GIVEN** a turn that spawned two subagents
- **WHEN** the user presses `F2` and selects the second child
- **THEN** the pane SHALL stream that child's assistant text and tool calls live and SHALL show its final status when it ends

#### Scenario: Foreign subagent subscription
- **WHEN** a connection calls `subagent.subscribe` for a session it has not attached
- **THEN** the response SHALL be `-32002` with `data.code` `session.not_attached`

### Requirement: Inline diff review for file edits (upgrade)
When a file-edit tool call requires approval, the approval card SHALL render the proposed change as a syntax-highlighted unified diff with numbered hunks and offer `accept all`, `reject`, `accept selected hunks` and `edit then accept`; the answer SHALL be sent as `approval.respond {id, decision, accepted_hunks?, replacement?}`, the tool SHALL apply exactly the accepted hunks, and the tool result returned to the model SHALL state which hunks were applied and which were rejected.
**Milestone:** M3

#### Scenario: Partial acceptance
- **GIVEN** an edit proposal with hunks 1, 2 and 3
- **WHEN** the user accepts hunks 1 and 3
- **THEN** the file SHALL contain only those two changes and the tool result SHALL say hunk 2 was rejected by the user

### Requirement: Session timeline scrubber (upgrade)
The TUI SHALL offer a timeline (`/timeline` or `Ctrl+T`) listing the session's turns with timestamp, first line of input, tool count and cost; moving the cursor SHALL preview that turn read-only without changing the live session, and `rewind here` SHALL call `session.rewind {session_id, to_turn}`, which creates a new branch session ending at that turn (the original is kept) and, when file checkpoints exist for later turns, offers to restore the workspace to that turn's checkpoint.
**Milestone:** M3

#### Scenario: Rewind keeps the original
- **GIVEN** a session with 12 turns
- **WHEN** the user rewinds to turn 7
- **THEN** a new branch session containing turns 1 to 7 SHALL become active and the original 12-turn session SHALL remain listed unchanged

### Requirement: Shared live sessions (upgrade)
A session SHALL accept any number of simultaneous attachments with role `driver` (may submit, interrupt and answer requests) or `observer` (read-only); `prometheus tui --attach <session-id|title>` SHALL join a live session as driver and `--observe` as observer; every attached client SHALL receive `session.presence {clients: [{client_id, client_kind, role, since}]}` on each join or leave, and the TUI SHALL show who else is attached and label each input with its stamped author.
**Milestone:** M3

#### Scenario: Observer cannot submit
- **GIVEN** a TUI attached with `--observe`
- **WHEN** it sends `prompt.submit`
- **THEN** the response SHALL be `-32002 observers cannot submit input`

#### Scenario: Desktop joins a TUI session
- **WHEN** the desktop attaches to the session the TUI is driving
- **THEN** both clients SHALL receive `session.presence` listing both, and deltas of the running turn SHALL stream to both with identical `seq`

### Requirement: Orphaned attachments are reaped
When a connection drops, its attachments SHALL be released immediately but the session SHALL keep running; a session with no attachments, no running turn, no pending request and no queued input SHALL be closed after `daemon.idle_session_close_secs` (default 3600), and a turn whose last driver disappeared SHALL continue to completion with server requests settling as `no_responder`.
**Milestone:** M2

#### Scenario: Terminal window closed mid-turn
- **GIVEN** a turn running in a session attached only by one TUI
- **WHEN** the TUI's connection drops
- **THEN** the turn SHALL keep running and any approval it needs SHALL be denied as `no_responder`

# The daemon and its client protocol

One **gateway daemon** per home owns every served profile's state while it
runs. The CLI chat, the TUI, the desktop app, the dashboard and editor
integrations are all **clients** of it: they attach over one local
protocol instead of opening the session store themselves. This guide
explains how a client finds the daemon, attaches, and talks to it. The
[client protocol reference](../reference/rpc.md) lists every method, event
and type; it is generated from the same typed catalog the daemon serves,
as are `schema/rpc.json` and the TypeScript bindings `schema/rpc.ts`.

Every command below is run by the documentation tests against a fresh home,
in order. Lines starting with `$` are commands; the lines after a command
are (part of) its output, with `<...>` standing for text that varies.

## Running the daemon

```console
$ prometheus config set model.provider mock
$ prometheus config set model.default mock
$ prometheus daemon start
daemon started (pid <pid>, <url>)
$ prometheus daemon start
gateway already running (pid <pid>)
```

`daemon start` is idempotent; `daemon run` serves in the foreground, and a
second daemon process for the same home exits 75 naming the first.
`daemon status` prints the process state, `daemon stop` lets running turns
finish and stops it, and `daemon restart` (after an update) drains running
turns for up to `daemon.restart_drain_secs` and starts this binary's
daemon. While it drains, the old daemon refuses new work with `-32006`
`daemon is restarting; reconnect to continue`.

Daemon-wide settings (`daemon.rpc.*` and the rest of the
[daemon configuration](../reference/daemon-configuration.md)) are read only
from `<home>/daemon.toml`, never from a profile.

## Which profiles it serves

By default the daemon serves its launch profile only (`-p NAME`, else the
home's `active_profile`, else `default`). With `gateway.multiplex = true` in
the launch profile it serves every profile of the home except those with
`gateway.standalone = true`, each through its own isolated scope (one whose
configuration does not load is logged and skipped). A client asking for a
profile the daemon does not serve gets no ticket; since the daemon holds
the home, a chat on such a profile exits 75 until the daemon serves it or
stops. Each served profile's `config.toml` is reloaded when it changes (see
[live reload](configuration-and-profiles.md#live-reload)).

## Finding and attaching to the daemon

1. **Discovery.** The daemon writes `<home>/run/gateway.json` (mode 0600)
   with its `pid`, `instance_id`, `build_id`, `rpc_url` (a loopback
   WebSocket URL), `control` (the control channel), `rpc_protocol` and
   `started_at`, and removes it on a clean stop. A file whose pid is not
   alive (or was reused by another process) is stale: clients treat the
   daemon as not running and never dial its URL. Clients always read
   `control` from this file; they never rebuild the path.
2. **Ticket.** Over the control channel (a Unix socket only the owning
   user can open, or a named pipe on Windows), the client sends one JSON
   line `{"op": "ticket", "purpose": "interactive", "profiles": ["default"]}`
   and reads one JSON line back. A ticket is single-use, expires after 30 s
   and grants exactly one profile (only the `profile-admin` purpose may
   grant several).
3. **Upgrade.** The client opens `rpc_url` with the WebSocket subprotocols
   `prometheus-rpc.v<major>, prometheus-ticket.<ticket>`. The ticket never
   appears in a URL, a log line or a file. An unknown, expired or reused
   ticket closes the connection with 4401, a foreign `Host` or any `Origin`
   with 4403.
4. **Handshake.** The first request must be `rpc.hello` with the client
   kind, its version, the protocol it speaks and its capabilities; anything
   else first fails with `-32008 handshake required`. A different protocol
   major fails with `-32008` naming both versions and closes with 4426. A
   lower minor keeps working: the daemon withholds fields and events the
   client did not know about.

The answer names the daemon's `build_id`; a client built differently
prints `daemon is running an older build (<version>); restart with
'prometheus daemon restart'` and keeps working when the major matches.

## Requests, errors and close codes

Frames are JSON-RPC 2.0, one message per text frame, at most
`daemon.rpc.max_frame_bytes` (a larger frame closes with 4413). Params are
validated against the method's contract before any handler runs: an
undeclared key fails with `-32602` and `data.path` pointing at it. Every
error carries `data: {kind, code, hint?}`, where `kind` is the error
taxonomy kind (it fixes the CLI exit status) and `code` the stable dotted
error code. The [reference](../reference/rpc.md#error-codes) lists the
error and close codes.

`rpc.ping` is answered from the connection's read loop. Requests the
catalog marks `long` (`session.create`, `session.resume`, `session.list`,
`session.compact`, `session.transcript`, `session.resolve`, `session.fork`,
`command.dispatch`, `complete.path`, `model.options`) run as tasks of
their own on the worker pool (`daemon.rpc.workers`) and may answer out of
order; every other request of a connection is answered in arrival order.
A handler that fails unexpectedly answers `-32099 internal error:
<method>` and the connection stays open. An upgrade to any path but
`/rpc` closes with 4404.
Clients ping every 15 s and treat 45 s of silence as a dead connection;
the daemon closes an idle connection with 4408.

## Sessions and events

`session.create` creates a session in the ticket's profile and attaches the
connection to it; `session.resume` loads a stored one and attaches;
`session.attach` and `session.detach` join and leave. `session.resolve`
turns a chat's `-r`/`-c` reference into a session id, `session.fork` copies
a session's active messages up to a chosen one into a new session (the
parent is unchanged), `session.tree` lists a session's descendants (forks,
compression continuations, delegates) with their relation and
`session.siblings` the other children of its parent, and
`session.interrupt` stops the running turn.
`memory.summary` answers a session's memory review line (`null` when the
agent changed nothing to review), which a client shows as it leaves or
resumes the session, and `autoprune.offer` answers the low-disk prune
question at most once a day for an interactive client's startup (answered
with `/autoprune on|off|later` through `command.dispatch`). The daemon
cannot push either: a resumed session's events start after the client
attaches, and an ending one has no client left.
Creating or resuming with `exclusive: true` makes the connection the
session's interactive owner: until it detaches or disconnects, another
exclusive resume fails with `-32004` `session <id> is open in another
client (pid <n>)` (attaching read-only and forking still work). Attaching
to a session of another profile is refused with `-32002` exactly like a
session that does not exist. Every other method that names a session
(events, transcript, completion, catalog, subagents, fork, changes) needs
the connection attached to it (`-32002` with `data.code`
`session.not_attached`), and the ones that drive it (`prompt.submit`,
`command.dispatch`, `session.compact`, `session.interrupt`,
`approval.respond`) need it attached as a `driver`, the default role of
`session.attach {session_id, role}`; an `observer` only reads.
`session.create` retried with the same `idempotency_key` within 300 s
returns the same session; its `source`, when given, must be the
connection's own client kind. Methods that act on a profile take
`profile` and need it when the ticket grants several.

`session.create` and `session.resume` take a `setup`: the session-shaping
flags of a chat (`model`, `provider`, `set` overrides, `skills`, `yolo`,
...), applied when the session loads (resuming a loaded, idle session with
another setup reloads it). `prompt.submit` takes `images` (`{"path":
"/abs/file.png"}` read by the daemon, or `{"blob": "blob:sha256:..."}` of
an image the session already holds), `files` (absolute paths of other
received files the daemon keeps in the session's blob store and, with
`attachments.storage = "workspace"`, copies into the workspace) and
`run_budget_secs`.

`command.dispatch {session_id, name, args}` runs a slash command on the
session with the same implementation as the line interface, following the
command's busy policy while a turn runs, and answers `{"type": "done",
"notice"}` (its output), `{"type": "send", "message", "images"}` (submit
it as a prompt: `/retry`, `/learn`, skill commands) or `{"type":
"refused", "reason"}` (commands that belong to the client, such as
`/exit` or `/theme`, and usage errors). A notice a command posts later
(`/refine`'s summary) arrives as a `session.event` of type
`command.notice` with `payload.text`. A background process notice of an
interactive session (a `notify_on_complete` or quiet-wait exit, a watched
line, a heartbeat) arrives as a `session.event` of type `process.notice`
with `payload.kind` (`result`, `error` or `progress`) and `payload.text`,
shown as its own message; gateway sessions get theirs in the chat instead,
and one-shot sessions get none (quiet-wait never switches there). While a
turn generates its answer, each piece of text arrives as a `session.event`
of type `assistant.delta` with the turn's `turn_id` (as `prompt.submit`
answered it) and `payload.text`; it is not a trajectory event, and the
whole answer still arrives as `assistant.message` and in `turn.finished`
(voice clients speak the reply from these pieces).

`prompt.submit` never drops an input: while a turn runs it follows
`busy.input_mode` (queued, steered into the running turn, or run after
interrupting it) and says how in `admission` (`started`, `queued`,
`steered`, or `handled` when no turn runs: the `yes` that saves the job a
`/cron` echo just showed, with the outcome in `notice`); a resend with the same `input_id` returns the original
admission. The gateway stamps each input's author (client kind, client id,
principal) into its stored message; a client-supplied `author` is refused
with `-32002 rpc.forged_author`.

`commands.catalog`, `complete.slash`, `complete.path` (listed through the
session's own file backend), `model.options`, `subagent.list` and
`config.get` read without touching the live session's system prompt,
toolset or memory snapshot.

`prompt.submit` returns a `turn_id` at once; the turn's output arrives as
`session.event` events (one per trajectory event) and ends with
`turn.finished`, whose `report` is the turn's `--usage-file` document. Every event carries a per-session `seq` and the daemon's
`epoch`. After a dropped connection, `session.events.since {session_id,
last_seen}` returns the kept events after `last_seen` (the newest 512
events or 4 MiB per session), whether older ones were evicted
(`truncated`), the requests still waiting for an answer, and the current
`epoch`: a different epoch means the daemon restarted, and the client
resets its watermark. `session.attach` with `last_seen_seq` and `epoch`
does the same while re-attaching: the missed events are sent before its
answer, or, when some were evicted or the epoch changed, its answer says
`truncated: true` and the client reloads `session.transcript`. A session
no client has attached to for `daemon.idle_session_close_secs` (3600)
that runs nothing is closed.

## Server-to-client requests

When a turn needs a person (an approval, a clarifying question, a secret
or a sudo password) the daemon sends a JSON-RPC request with an id
`srq-<12 hex>` to the attached connections that declared
`capabilities.server_requests` in `rpc.hello` and attached as drivers,
with its `deadline_ms`. The first answer wins; every recipient then
receives `request.resolved {id, by_client}`. A request that times out
(approvals after `approvals.timeout_secs`, others after
`daemon.rpc.request_timeout_secs`) or is cancelled emits exactly one
`request.cancel`, and a later answer gets `-32004 request <id> already
settled`. With no capable driver attached, or when the last one leaves,
the request settles at once as `no_responder` (a `request.cancel` with
that reason), which denies an approval; the turn keeps running.

## Talking to the daemon by hand

`prometheus rpc call <method> [<json-params>]` performs the handshake for
the selected profile, sends one request and prints the result as JSON; an
error prints the JSON-RPC error object on stderr and exits with the status
of its kind. `prometheus rpc events [--session ID] [--since SEQ]` prints
event frames as JSON lines until interrupted, and `prometheus rpc schema`
prints the schema this binary serves.

```console
$ prometheus rpc call session.create
"session_id": "ses_<id>"
$ prometheus rpc call sesion.list # exits 2
"message": "unknown method: sesion.list"
hint: did you mean 'session.list'?
$ prometheus daemon stop
daemon stopped
```

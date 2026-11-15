# Agents on other machines: A2A and peer gateways

A Prometheus agent talks to agents outside its own daemon in three ways:

- the open [Agent2Agent (A2A)](https://a2a-protocol.org) protocol, in both
  directions: an inbound A2A server any A2A client can call, and the
  outbound `a2a_*` tools the model uses to call other A2A agents;
- **peer gateways**: other Prometheus daemons reached over their API
  server, for direct messages and remote runs in their bots' Bot Chats;
- the **desktop relay**: a desktop holding several connections carries bot
  messages between their daemons, which never see each other's
  credentials.

Every crossing is authenticated per peer and fails closed, and whatever a
peer says reaches the model as untrusted input, never as instructions.

## The A2A server

The server is off by default. A profile opts in, and the daemon serves it
on the daemon-wide listener of `daemon.toml` (default `127.0.0.1:9900`) at
`/p/<profile>/`, and at `/` for `default`:

```toml
# <profile>/config.toml
[a2a.server]
enabled = true
trusted_peers = ["laptop"]

[[a2a.server.peers]]
name = "laptop"
token_secret = "A2A_LAPTOP_TOKEN"   # prometheus secrets set A2A_LAPTOP_TOKEN
```

A non-loopback listener (`[a2a] listen = "0.0.0.0:9900"`) needs `[a2a]
public = true` *and* a peer token in every served profile; otherwise the
listener does not start ("non-loopback a2a.listen requires a2a.public =
true and peer tokens"). A profile without tokens answers only requests
addressed to a loopback name (or the host of `a2a.server.public_url`) and
refuses anything a browser sends (an `Origin` header, a cross-site fetch,
a form post) with 403, so a web page cannot drive your agent.

Peers find the agent through its Agent Card at
`/.well-known/agent-card.json` (and the legacy `/.well-known/agent.json`):
its name (`a2a.server.agent_name`, default `<hostname>-<profile>`), the URL
to call (`a2a.server.public_url` behind a reverse proxy), streaming and
push-notification support, the bearer scheme and one skill per toolset
inbound sessions get (narrowed by `a2a.server.advertised_toolsets`).

The JSON-RPC endpoint speaks A2A v1.0 (`SendMessage`,
`SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`,
`SubscribeToTask` and the push-notification configuration methods) and
the pre-1.0 aliases (`message/send`, `tasks/get`, ...); it is tested
against the official A2A Python SDK. A task waits up to
`a2a.server.reply_timeout_secs` (default and minimum 300) for the reply,
which becomes the task's artifact.

### Who may talk, and how much

- A bearer token names the caller: a `[[a2a.server.peers]]` token its peer,
  `a2a.server.shared_token_secret` the caller's IP. A bad token is 401.
- Only identities in `a2a.server.trusted_peers` start turns (unless
  `allow_all_peers = true`, warned about on every start); anyone else gets a
  task in state `rejected` and no turn.
- Each identity is limited to `rate_limit_per_minute` requests (429), and
  each conversation to `max_pingpong_turns` exchanges (default 5, at most
  20) per 10 minutes, so two agents cannot talk to each other forever: the
  next message is `rejected` with reason `pingpong_cap`.

### What an inbound message can do

Each conversation runs in its own session, keyed by the exact (peer,
tenant, context id) of the request: two peers reusing a context id never
share a transcript, and a context id is never matched by title. Imported
Hermes A2A sessions, whose binding cannot be proven, stay read-only and are
reported by `prometheus doctor`.

The message reaches the model framed as untrusted text from the named peer,
after the prompt-injection scanner looked at it; it is never a slash
command (`/model gpt-5` from a peer is just text). Inbound sessions get the
toolsets of `a2a.server.toolsets` (default `api_server`), every approval is
denied, replies are redacted, and every exchange (peer, context, task,
direction, sizes, outcome, but not the text unless `audit_content = true`)
goes to `<profile>/logs/security-audit.jsonl`.

A peer may register a push URL for a task; status changes are POSTed there
signed with HMAC-SHA256 over `<timestamp>.<body>` (`X-A2A-Signature:
sha256=<hex>`, `X-A2A-Timestamp`) with `a2a.server.push_secret` or the
peer's own token. Push URLs obey the egress policy: a private, loopback or
link-local address such as `http://169.254.169.254/hook` is refused unless
allowlisted. A failed push is retried 3 times (2, 4, 8 s) and then shown on
the task.

## Calling other A2A agents

The `a2a` toolset is off by default; enable it like any toolset
(`tools.enabled_toolsets = ["core", "a2a"]`, or per surface). Agents are
configured by name:

```toml
[a2a.agents.researcher]
url = "https://research.example.com"
auth_secret = "RESEARCHER_TOKEN"
timeout_secs = 120
capabilities = ["papers"]
```

- [`a2a_discover`](../reference/tools.md#a2a_discover) fetches and
  summarizes an Agent Card;
- [`a2a_call`](../reference/tools.md#a2a_call) sends a message to a
  configured name or a URL and returns the reply with its `context_id`
  (pass it back to continue the conversation); an unknown name is refused
  with the configured names;
- [`a2a_list`](../reference/tools.md#a2a_list) shows the agents, saved
  conversations and per-agent call metrics;
- [`a2a_history`](../reference/tools.md#a2a_history) shows a conversation's
  exchanges, persisted in `state.db`;
- [`a2a_orchestrate`](../reference/tools.md#a2a_orchestrate) sends one
  message to every agent advertising a capability (at most 8): `all`
  returns every reply, `first` the first success, `best` the reply an
  auxiliary judge ranks highest with all replies attached.

Each call needs a `net.connect` grant for the agent's host and goes through
the egress policy; replies come back as untrusted tool output.

## Peer gateways

A peer is another machine's Prometheus daemon with its [API
server](api-server.md) enabled. Register it with the peer profile's API key
on stdin (never as an argument, so it stays out of shell history and logs);
it is stored as the secret `PEER_<NAME>_KEY`:

```console
$ prometheus gateway peer list
no peers (prometheus gateway peer add <name> --url <url>)
$ echo -n "$TOKEN" | prometheus gateway peer add spark --url http://127.0.0.1:9 --note "lab box"
peer spark registered (http://127.0.0.1:9; probe: <...>)
$ prometheus gateway peer list
spark http://127.0.0.1:9 last probe: <...> (lab box)
$ prometheus gateway peer dm spark "status?" # exits 69
$ prometheus gateway peer remove spark
peer spark removed
$ prometheus gateway peer dm spark "status?" # exits 66
```

Registration probes `GET /v1/capabilities`: an unreachable peer is only a
warning, but a 401 exits 78 ("peer spark rejected the key") and registers
nothing.

`peer dm <peer>[/<agent>] [message]` (the message from stdin when absent)
sends one message to the remote agent's canonical Bot Chat (`<agent>` on a
multiplexed peer, its default profile otherwise) and prints the reply. A
turn that outlives the connection, or runs longer than 300 seconds, prints
`message delivered to spark's Bot Chat; turn still running (run <id>)`:
the peer has it, so do not resend. `peer run` submits an asynchronous run
and prints its id; with `--idempotency-key` a retry returns the same run
and the peer runs the task once. `peer status` and `peer stop` follow and
stop one run. Exit codes: 0 done, 1 the peer refused, 2 usage, 66 unknown
peer or agent, 69 unreachable, 78 key missing or rejected.

## One roster across gateways

`bot.roster.list` with `include_remote` merges this daemon's profiles, each
peer's `GET /api/agents` (listed again after 300 seconds, or with
`refresh`) and the agents a desktop reports for its other connections. A
name served by several sources gets `@<name>-<device>` (`@research-local`,
`@research-homelab`); a unique one keeps `@<name>`. A peer that cannot be
reached keeps its last rows, marked `stale` with the error, and a remote
`default` profile is listed under its bot title or `<hostname>@<peer>`.

## The desktop relay

When a bot message targets an agent on another desktop connection that no
peer entry reaches, the sending daemon queues a signed envelope. The
desktop, holding both connections, drains it (`bot.relay.drain`), hands it
to the target daemon over that connection's own socket
(`bot.relay.deliver`), where it runs as the Bot Chat's next turn (bounded
by `bot_mode.relay_turn_timeout_secs`, default 600) once per delivery id,
and carries the reply back. An envelope drained but never answered is
offered once more after 1560 seconds; after 3120 seconds the sender gets
`delivery_timeout`. An envelope no courier drained within
`bot_mode.envelope_ttl_secs` (default 900) fails with `queued_expired`.

# The OpenAI-compatible API server

The gateway daemon can serve a profile's whole agent — its tools, memory,
skills and sessions — over HTTP to OpenAI-compatible clients: Open WebUI,
LobeChat, LibreChat, the OpenAI SDKs or `curl`. It offers Chat
Completions, the Responses API, a Runs API with approval and
clarification controls, and REST over sessions and scheduled jobs.

The server is off by default and fails closed: it starts only when a
profile enables it, and it serves a profile only with that profile's own
bearer key.

## Turning it on

Enable it in the profile and store a key of at least 32 characters in the
profile's secrets store (`openssl rand -hex 32 | prometheus secrets set
API_SERVER_KEY`), then restart the daemon:

```console
$ prometheus config set api_server.enabled true
$ prometheus config get api_server.key_secret
API_SERVER_KEY
```

A profile without a usable key is logged
(`api_server disabled for <profile>: missing or weak key`) and its routes
answer 404. `GET /health` answers `{"status":"ok"}` without a key.

The listener is machine-wide, in `<home>/daemon.toml`:

```toml
[api_server]
listen = "127.0.0.1:8642"   # the default
public = false              # a non-loopback address needs public = true
cors_origins = []           # exact browser origins, never "*"
public_host = "agent.example.com"  # a Host name accepted behind a proxy
```

On a loopback bind a request whose `Host` is not a loopback name (or
`public_host`) is refused with 403, so a web page that rebinds its DNS
name to 127.0.0.1 cannot reach the server. A request carrying an `Origin`
outside `cors_origins` is refused with 403 before any work starts. Every
answer carries `X-Content-Type-Options: nosniff` and
`Referrer-Policy: no-referrer`.

## Profiles and keys

Every route is reachable as `/p/<profile>/<route>`, and for the `default`
profile also without the prefix. A request authenticates with
`Authorization: Bearer <key>`, compared against the routed profile's key
only: another profile's key answers 401. Run, response, job and session
ids resolve only under the profile that created them.

To give several people their own agent, give each a profile (served by the
same daemon with `gateway.multiplex = true`) and connect each frontend to
`http://host:8642/p/<profile>/v1` with that profile's key.

`GET /v1/models` lists one model: `api_server.model_name`, or the profile
name (`prometheus-agent` for `default`), plus each
`[api_server.model_routes.<alias>]`.

## Chat Completions

`POST /v1/chat/completions` runs one agent turn for the last user message.
Clients send their whole history every time:

- with `X-Prometheus-Session-Id` (alias `X-Hermes-Session-Id`) the named
  API session continues from its stored history and earlier client
  messages are ignored;
- without it the conversation is the API session derived from the system
  prompt and the first user message.

The answer echoes the session in `X-Prometheus-Session-Id` and in a
`prometheus` block. `finish_reason` is `stop` on success, `length` with
`prometheus.error_code = "iteration_budget_exhausted"` when the turn ran out
of iterations and wrote a wrap-up, `cancelled` when it was stopped and
`error` on failure. User content may be text and `image_url` parts (remote
`http(s)` URLs, fetched under the profile's URL policy, or `data:image/*`
URLs); uploaded files are refused with 400 `unsupported_content_type`.
With `stream: true`, reasoning arrives as `delta.reasoning_content` and
tool starts as `prometheus.tool.progress` events (unless
`api_server.tool_progress_events = false`).

A caller's `system` message is layered after the profile's own system
prompt as a separate block, fixed when the session is created; it never
replaces the agent's identity, tools, memory or skills. A continuation that
sends different instructions gets them as a notice in that turn.
`X-Prometheus-Session-Key` (at most 256 characters) names a stable caller
identity: it scopes the memory provider across that caller's sessions and,
on Responses and Runs, picks the conversation to continue.

API sessions start from the `api_server` toolset (no `clarify`,
text-to-speech, computer use or messaging send), or from
`api_server.toolsets`. Chat Completions and Responses have no way to ask a
person, so a dangerous command is decided at once by
`api_server.unattended_approvals` (`deny` by default) and the agent gets a
`BLOCKED:` result.

### Choosing the model per request

A turn's route is, in order: the session's own `/model` choice, an
`api_server.model_routes` alias named by `model`, the request's `provider`
(with its `model`), then the profile default. A bare `model` without
`provider` (`gpt-4o` from a generic client) is ignored on Chat Completions
and Responses unless `api_server.direct_model_requests = true`. A
`provider` that contradicts a route alias, or an unknown provider, answers
400. `model_options.reasoning_effort` and `service_tier` (`priority` asks
for the fast tier) apply to that request only; the session's system prompt
and tool list stay the same.

## Responses

`POST /v1/responses` takes `input` (a string or message items),
`instructions`, `previous_response_id`, `conversation`,
`conversation_history` and `store` (default true). A response's `output`
lists only its own turn's items: executed `function_call` and
`function_call_output` items, `reasoning` summaries, commentary messages
(`phase: "commentary"`) and the final message. Output items a client sends
back as input (`reasoning` among them) are ignored. Stored responses live
in the profile's `state.db` (`GET`/`DELETE /v1/responses/{id}`), at most
`api_server.responses.max_stored` of them.

## Retries without double work

`Idempotency-Key` (1-255 visible ASCII characters) on Chat Completions,
Responses and Runs is reserved in the profile's store before any work
starts. An exact retry replays the original answer — as JSON or as a
stream, whatever the retry asks for — without running again; the same key
for different work answers 409 (`admission_conflict`, or
`idempotency_key_conflict` on Runs). A key whose work left no answer (the
daemon stopped mid-turn) answers 409 `unknown_execution` instead of running
twice.

## Runs

`POST /v1/runs` answers 202 with a `run_id` at once; the turn runs in the
background. `GET /v1/runs/{id}` reports its status (`queued`, `running`,
`waiting_for_approval`, `waiting_for_clarification`, `stopping`,
`completed`, `failed`, `cancelled`, `interrupted`, `unknown`), usage, the
`runtime` pair that served it after any fallback, and its pending controls.
`GET /v1/runs/{id}/events` streams `run.started`, `message.delta`,
`message.interim`, `reasoning.delta`, `tool.started`, `tool.completed`,
`approval.request`, `clarify.request`, `subagent.start`,
`subagent.complete` and one terminal `run.completed`, `run.failed`,
`run.cancelled` or `run.interrupted`; reconnect with `Last-Event-ID` to
continue after the last event you saw.

Controls: `POST /v1/runs/{id}/stop`, `/approval`
(`{request_id, execution_generation, choice}`), `/clarify`
(`{request_id, execution_generation, answer}`), `/steer` (`{text}`) and
`/resolve-unknown` (`{admission_id, execution_generation}`). An answer
with a stale `execution_generation` is refused and the request stays
pending. A run active when the daemon stops is recorded `interrupted`; one
left by a daemon that crashed is `unknown` until a client resolves it, and
later runs of its session wait until then. Each profile runs at most
`api_server.max_concurrent_runs` API turns at once (429 beyond).

## Sessions, jobs and discovery

- `/api/sessions`: list (`limit`, `offset`, `source`,
  `include_children`), create, show, `PATCH` (title, `end_reason`),
  `DELETE`, `/messages` (`inline_images=false` shows `[image]`),
  `/fork`, `/chat` and `/chat/stream`. Every session of the profile is
  readable; only API sessions take chat, `PATCH` or `DELETE`.
- `/api/jobs`: the profile's scheduled jobs with the [`cronjob`](../reference/tools.md#cronjob) tool's
  fields, confirmation and messages; jobs created here record origin `api`.
- `GET /v1/capabilities`, `/api/model/options`, `/v1/skills`,
  `/v1/toolsets`, and the authenticated `GET /health/detailed`, which
  reports bounded status and counts — never configuration values,
  credentials, paths or raw errors.

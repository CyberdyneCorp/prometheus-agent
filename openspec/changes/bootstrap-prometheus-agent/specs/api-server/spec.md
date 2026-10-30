# API Server

## Purpose
The API server is the daemon's opt-in HTTP surface that lets OpenAI-compatible clients (Open WebUI, LobeChat, the OpenAI SDKs, curl) and Prometheus-aware control planes use a profile's full agent — tools, memory, skills — over `/v1/chat/completions`, `/v1/responses`, a Runs API with approval and clarification controls, and REST over sessions and scheduled jobs. Its key decisions are: it is fail-closed (every profile needs its own bearer key, even on loopback), every request is bound to exactly one ProfileScope through the `/p/<profile>/` prefix, run-starting submissions are durably admitted with idempotency keys so retries never execute work twice, and API-owned sessions never write into a conversation owned by another surface. It also hosts the subscription proxy, a separate credential-attaching passthrough for raw model inference.

## ADDED Requirements

### Requirement: Opt-in listener that fails closed
The daemon SHALL serve the API server only when at least one profile sets `api_server.enabled = true` (default false), on the daemon-wide listener `api_server.listen` in `<home>/daemon.toml` (default `127.0.0.1:8642`), SHALL refuse to start the listener on a non-loopback address unless `api_server.public = true` (logging a WARN naming the address on every start), and SHALL serve a profile only when the secret named by its `api_server.key_secret` (default `API_SERVER_KEY`) exists in that profile's secrets store and is at least 32 characters, otherwise logging "api_server disabled for <profile>: missing or weak key" and answering that profile's routes with 404; unauthenticated `GET /health` and `GET /v1/health` SHALL return `{"status":"ok"}` and nothing else.
**Milestone:** M2

#### Scenario: Enabled without a key
- **GIVEN** profile `default` sets `api_server.enabled = true` but has no `API_SERVER_KEY` secret
- **WHEN** the daemon starts
- **THEN** the system SHALL log the missing-key error, serve no agent routes for `default`, and `GET /health` SHALL still return `{"status":"ok"}`

#### Scenario: Public bind needs explicit opt-in
- **WHEN** `api_server.listen = "0.0.0.0:8642"` without `api_server.public = true`
- **THEN** the system SHALL fail validation with "non-loopback api_server.listen requires api_server.public = true"

### Requirement: Profile-bound routing and bearer authentication
Every API route SHALL be reachable as `/p/<profile>/<route>` and, for the `default` profile only, also without prefix; a request SHALL authenticate with `Authorization: Bearer <key>` compared in constant time against the routed profile's own key only (another profile's key on that prefix answers 401), failures SHALL answer HTTP 401 with an OpenAI-style error body `{"error":{"type":"authentication_error","code":"invalid_api_key"}}` without revealing whether the profile exists, a prefix naming an unserved profile SHALL answer 404, and every run, response, job and session id SHALL be resolvable only under the profile that created it (another profile's id answers 404, never 403).
**Milestone:** M2

#### Scenario: Default key on a named prefix
- **GIVEN** profiles `default` and `alice` with distinct keys
- **WHEN** a client calls `/p/alice/v1/models` with `default`'s key
- **THEN** the system SHALL answer 401

#### Scenario: Run id from another profile
- **GIVEN** run `run_01J...` created under `/p/alice/`
- **WHEN** `GET /p/bob/v1/runs/run_01J...` is called with `bob`'s key
- **THEN** the system SHALL answer 404

### Requirement: Browser access, Host checks and security headers
The API server SHALL not emit CORS headers unless `api_server.cors_origins` (daemon-wide, default empty) lists exact origins (`*` fails validation); with an allowlist it SHALL answer preflights with `Access-Control-Max-Age: 600`, allow the request headers `Authorization`, `Content-Type`, `Idempotency-Key`, `X-Prometheus-Session-Id` and `X-Prometheus-Session-Key`, and include CORS headers on SSE responses; a request carrying an `Origin` not in the allowlist SHALL be refused with 403; on a loopback bind a request whose `Host` is not a loopback name or the configured `api_server.public_host` SHALL be refused with 403 (DNS-rebinding defence); every response SHALL carry `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.
**Milestone:** M2

#### Scenario: Web page on another origin
- **GIVEN** no CORS allowlist
- **WHEN** a browser page at `https://evil.example` POSTs to `/v1/chat/completions` with a stolen key
- **THEN** the system SHALL answer 403 and start no run

#### Scenario: Rebinding host refused
- **GIVEN** a loopback bind
- **WHEN** a request arrives with `Host: attacker.example:8642`
- **THEN** the system SHALL answer 403

### Requirement: Model listing and Open WebUI compatibility
`GET /v1/models` SHALL return an OpenAI list object with exactly one model whose `id` is `api_server.model_name` or, by default, the profile name (`prometheus-agent` for `default`), `owned_by` `prometheus`, plus one entry per configured `api_server.model_routes` alias; the endpoints SHALL accept the request shapes sent by OpenAI-compatible frontends (Open WebUI in Chat Completions and Responses modes, LobeChat, LibreChat, the OpenAI Python and JavaScript SDKs), including full client-managed history on every request, `stream: true`, unknown optional OpenAI fields (ignored), and a model value naming the advertised id; several users SHALL be served by giving each a profile and connecting each frontend connection to that profile's `/p/<profile>/v1` base URL with its key.
**Milestone:** M2

#### Scenario: Two users, two models
- **GIVEN** profiles `alice` and `bob` with the API server enabled
- **WHEN** Open WebUI adds connections `http://host:8642/p/alice/v1` and `http://host:8642/p/bob/v1`
- **THEN** its model picker SHALL show `alice` and `bob`, each backed only by that profile's memory, skills and sessions

#### Scenario: Unknown OpenAI field ignored
- **WHEN** a Chat Completions request includes `"logit_bias": {}`
- **THEN** the system SHALL ignore the field and answer normally

### Requirement: Chat Completions endpoint
`POST /v1/chat/completions` SHALL run one agent turn for the last user message and return a `chat.completion` object (or `chat.completion.chunk` SSE frames ending in `data: [DONE]` with `stream: true`) whose `finish_reason` is `stop` on success, `length` with `prometheus.error_code = "iteration_budget_exhausted"` when the turn ran out of its iteration budget but produced a wrap-up, `cancelled` when interrupted and `error` on failure, never `stop` for a failed or cancelled turn; user content MAY be a string or `text` and `image_url` parts (remote `http(s)` or `data:image/*` URLs), any `file`, `input_file`, `file_id` or non-image data URL SHALL answer 400 `unsupported_content_type`, reasoning SHALL appear as `delta.reasoning_content` (streaming) or `message.reasoning_content`, tool starts SHALL be emitted as named SSE events `prometheus.tool.progress` unless `api_server.tool_progress_events = false`, and `usage` SHALL include prompt, completion, cache-read and cache-write tokens.
**Milestone:** M2

#### Scenario: Budget exhausted is not success
- **GIVEN** a turn that hits `agent.max_turns` and writes a wrap-up
- **WHEN** it completes over Chat Completions
- **THEN** the response SHALL carry `finish_reason = "length"` and `prometheus.error_code = "iteration_budget_exhausted"`

#### Scenario: Uploaded file refused
- **WHEN** a request contains a content part of type `file`
- **THEN** the system SHALL answer 400 with code `unsupported_content_type` and start no turn

### Requirement: API session identity and ownership
A Chat Completions request SHALL continue the API session named by `X-Prometheus-Session-Id` (alias `X-Hermes-Session-Id`) when present — loading history from the session and ignoring earlier client messages — and otherwise SHALL derive a stable session from a hash of the caller's system prompt and first user message, never reusing the id of a deleted session; sessions created by the API server SHALL have `source = "api"`, the response SHALL echo the session id in a header and in SSE frames, and a request naming a session owned by another surface (CLI, TUI, desktop, ACP, messaging chat) SHALL be refused with 409 `permission_denied` before any admission or idempotency reservation.
**Milestone:** M2

#### Scenario: Header continues the session
- **GIVEN** an API session `ses_A` with two prior turns
- **WHEN** a client sends one new user message with `X-Prometheus-Session-Id: ses_A`
- **THEN** the agent SHALL see the two prior turns from storage plus the new message

#### Scenario: Foreign session refused
- **GIVEN** a Telegram chat session `ses_T`
- **WHEN** a client posts with `X-Prometheus-Session-Id: ses_T`
- **THEN** the system SHALL answer 409 `permission_denied` and run nothing

### Requirement: Caller instructions and long-term memory scoping
A caller `system` message (Chat Completions) or `instructions` field (Responses, Runs) SHALL be layered after the profile's own system prompt as a separate caller-instructions block fixed when the session is created, never replacing identity, tools, memory or skills, and a continuation that sends different instructions SHALL receive them as an attributed system notice in that turn rather than a rewritten system prompt; `X-Prometheus-Session-Key` (alias `X-Hermes-Session-Key`, at most 256 characters, control characters refused with 400) SHALL bind the request to a stable caller identity used as the memory-provider scope and, on Responses and Runs, as the conversation to continue when no explicit session or response chain is given, and SHALL be echoed on responses and SSE frames.
**Milestone:** M2

#### Scenario: Instructions do not replace the agent
- **GIVEN** Open WebUI sends system prompt "Always answer in French"
- **WHEN** the user asks to list files
- **THEN** the agent SHALL still have its terminal and file tools and SHALL answer in French

#### Scenario: Session key scopes memory
- **GIVEN** two requests from different chats with `X-Prometheus-Session-Key: webui:user-42`
- **WHEN** the memory provider recalls facts
- **THEN** both SHALL use the same memory scope for user-42

### Requirement: Responses endpoint and response store
`POST /v1/responses` SHALL accept `input` (string or items with `input_text` and `input_image`), `instructions`, `previous_response_id`, `conversation` (a named chain resolving to its latest response), `conversation_history` and `store` (default true), and SHALL return a `response` object whose `output` lists only this turn's items — already-executed `function_call` and `function_call_output` items with `status: "completed"`, `reasoning` items with summaries, mid-turn commentary as `message` items with `phase: "commentary"`, and the final `message` — with `status` `completed`, `incomplete` (iteration budget), `cancelled` or `failed`; streaming SHALL use OpenAI Responses event types with monotonic `sequence_number`; stored responses SHALL live in the profile's `state.db`, chained requests SHALL share one session, `GET` and `DELETE /v1/responses/{id}` SHALL read and remove one, the store SHALL keep at most `api_server.responses.max_stored` (default 100) responses with LRU eviction, and `api_server.responses.history_tool_output_max_chars` (default 0 meaning verbatim) SHALL cap each stored tool output to its head plus `...[<n> more chars]`.
**Milestone:** M2

#### Scenario: Chained turn
- **GIVEN** response `resp_1` ran `terminal` with `ls`
- **WHEN** a client posts `{"input":"Now show me the README","previous_response_id":"resp_1"}`
- **THEN** the agent SHALL see the earlier tool call and result and the new response's `output` SHALL NOT repeat them

#### Scenario: Echoed reasoning items ignored on input
- **WHEN** a client sends a previous `output` list containing `reasoning` items as the next `input`
- **THEN** the system SHALL ignore those items rather than treating them as user turns

### Requirement: Idempotent admission for Chat Completions and Responses
`POST /v1/chat/completions` and `POST /v1/responses` SHALL accept `Idempotency-Key` (1–255 visible ASCII characters, otherwise 400) scoped to the routed profile and a fingerprint of the presenting key, durably reserved in `state.db` before execution; an exact retry (same identity fields: for Responses `input`, `instructions`, `previous_response_id`, `conversation`, `conversation_history`, `model`, `provider`, `model_options`, `tools`, `truncation`; for Chat Completions the last user message, earlier messages unless a session header is sent, the system prompt, `model`, `provider` and `model_options`) SHALL replay the original answer in the requested delivery form (JSON or stream) without running again, a changed identity or a different target session SHALL be refused with 409 `admission_conflict` (in an already-open stream as a final chunk with `finish_reason: "error"`), a key whose claim exists without a replayable answer SHALL answer 409 `unknown_execution`, and admissions of a deleted session SHALL be kept as tombstones for 7 days so the key cannot run new work during that window.
**Milestone:** M2

#### Scenario: Retry after network loss
- **GIVEN** a request with `Idempotency-Key: k1` completed but the client lost the response
- **WHEN** the client resends the identical request with `stream: true`
- **THEN** the system SHALL stream the original answer and SHALL NOT start a second turn

#### Scenario: Key reused for different work
- **WHEN** a client sends `Idempotency-Key: k1` with a different `input`
- **THEN** the system SHALL answer 409 `admission_conflict` and run nothing

### Requirement: Runs API lifecycle
`POST /v1/runs` SHALL accept `input`, optional `session_id`, `instructions`, `conversation_history` (authoritative when non-empty), `previous_response_id`, `model`, `provider` and `model_options`, and SHALL answer 202 `{run_id, status}` with `run_id` `run_<ULID>`; `GET /v1/runs/{run_id}` SHALL report `status` in `queued`, `running`, `waiting_for_approval`, `waiting_for_clarification`, `stopping`, `completed`, `failed`, `cancelled`, `interrupted` or `unknown`, plus `session_id`, `output`, `usage` (with cache read and write tokens), `runtime {provider, model, route_source, requested?}` naming the pair that actually served the turn after any fallback, `pending_controls` (each with `request_id` and `execution_generation`) and, while the daemon drains, `shutdown_requested_at`; a run active at shutdown SHALL be persisted `interrupted` ("Gateway shutdown interrupted the run.") before the agent is stopped, a claimed run whose owner crashed SHALL report `unknown` (not terminal), and while the daemon drains or the store is locked past its write timeout run creation and controls SHALL answer 503 `runtime_draining` or `storage_unavailable` with `Retry-After`; terminal run records SHALL be retained for at least 24 hours.
**Milestone:** M2

#### Scenario: Graceful stop mid-run
- **GIVEN** a running run
- **WHEN** the daemon drains past its timeout
- **THEN** `GET /v1/runs/{id}` SHALL report `interrupted` with error "Gateway shutdown interrupted the run." and a late result SHALL NOT overwrite it

#### Scenario: Fallback reflected in runtime
- **GIVEN** a run that switched to a fallback provider
- **WHEN** it completes
- **THEN** `runtime.provider` and `runtime.model` SHALL name the fallback pair

### Requirement: Run idempotency ledger
`POST /v1/runs` with `Idempotency-Key` (1–255 visible ASCII characters) SHALL durably reserve the key per profile and key fingerprint before starting work, SHALL answer an identical retry with the original `run_id`, HTTP 202 and `Idempotency-Replayed: true` even after a daemon restart and after the run reached a terminal state, SHALL answer the same key with a different payload with 409 `idempotency_key_conflict`, SHALL retain keys for 24 hours after the run's last status change, and SHALL keep no reservation for a run that could not be admitted so that its exact retry is safe; requests without the header SHALL always create a new run.
**Milestone:** M2

#### Scenario: Replay after restart
- **GIVEN** run `run_X` created with `Idempotency-Key: ticket-123` and the daemon restarted
- **WHEN** the client retries with the same key and body
- **THEN** the system SHALL answer 202 with `run_X` and `Idempotency-Replayed: true`

### Requirement: Run events stream
`GET /v1/runs/{run_id}/events` SHALL stream SSE events `run.started`, `message.delta`, `message.interim` (`text`, `already_streamed`; gated by `gateway.display.interim_assistant_messages`, default true), `reasoning.delta`, `tool.started` (`tool`, argument `preview`), `tool.completed` (`tool`, `duration_ms`, `error`, result `preview` passed through forced secret redaction and truncated to 500 characters), `approval.request`, `clarify.request`, `subagent.start`, `subagent.complete` (status, summary, duration, tokens, cost, `child_session_id`, `delegation_id`) and exactly one terminal `run.completed`, `run.failed`, `run.cancelled` or `run.interrupted` carrying `usage` and `runtime`; per-tool subagent events SHALL NOT be forwarded, every SSE stream SHALL send a `: keepalive` comment after 10 seconds without events, a client MAY reconnect with `Last-Event-ID` to resume from the buffer, and unconsumed buffers SHALL expire 5 minutes after the last subscriber left without affecting the run itself.
**Milestone:** M2

#### Scenario: Approval refusal distinguishable
- **GIVEN** a tool call denied by approval
- **WHEN** its `tool.completed` event is emitted
- **THEN** it SHALL carry `error: true` and a preview starting `BLOCKED:`

#### Scenario: Reconnect without loss
- **GIVEN** a client disconnected after event id 41
- **WHEN** it reconnects within 5 minutes with `Last-Event-ID: 41`
- **THEN** the system SHALL resume from event 42

### Requirement: Run controls
The Runs API SHALL provide `POST /v1/runs/{id}/stop` (answers `{"status":"stopping"}` at once; the run stays `stopping` until its work exits, then `cancelled`), `POST /v1/runs/{id}/approval` with exactly `{request_id, execution_generation, choice}` where `choice` is `once`, `session`, `always` or `deny`, `POST /v1/runs/{id}/clarify` with `{request_id, execution_generation, answer}`, `POST /v1/runs/{id}/steer` with `{text}` injecting steering input under the busy rules, and `POST /v1/runs/{id}/resolve-unknown` with exactly `{admission_id, execution_generation}` that settles an `unknown` run as `interrupted`, releases the session's FIFO without replaying the lost turn, and returns the same receipt on an exact repeat; a stale or foreign identity SHALL answer 409 `stale_generation` or `not_found`, an extra or missing body field 400 `invalid_params`, and `stop` on an `unknown` run 409 `unknown_execution`; approval and clarification answers SHALL resolve the same pending request seen by attached WebSocket clients.
**Milestone:** M2

#### Scenario: Stale approval generation
- **GIVEN** a pending approval with `execution_generation` 3
- **WHEN** a client answers with generation 2
- **THEN** the system SHALL answer 409 `stale_generation` and the approval SHALL stay pending

#### Scenario: Unknown run resolved
- **GIVEN** a run reporting `unknown` after a crash and a queued follower turn
- **WHEN** the client posts the matching `admission_id` and `execution_generation` to `resolve-unknown`
- **THEN** the run SHALL become `interrupted` and the follower SHALL start without the lost turn being replayed

### Requirement: Unattended approvals and the API toolset
API-started sessions SHALL default to the `api-server` toolset — the profile's core tools minus `clarify`, text-to-speech, computer use, messaging send and bot-messaging tools — unless `api_server.toolsets` lists toolsets within the profile's enabled set; on Chat Completions and Responses, which have no answer channel, an approval request SHALL be resolved by `api_server.unattended_approvals` (`deny`, the default) immediately and reported to the model as a `BLOCKED:` tool result, while Runs and session-chat requests SHALL expose it as a pending control that waits up to `approvals.timeout_secs` and is denied on timeout.
**Milestone:** M2

#### Scenario: Approval over Chat Completions
- **GIVEN** a Chat Completions turn that calls a command requiring approval
- **WHEN** the approval is requested
- **THEN** the system SHALL deny it immediately and the agent SHALL receive a `BLOCKED:` result

### Requirement: Per-request model selection
`POST /v1/chat/completions`, `/v1/responses`, `/v1/runs` and the session-chat endpoints SHALL accept `model`, `provider` and request-scoped `model_options` (reasoning effort, service tier) and resolve the turn's runtime in this order: the session's `/model` override, an `api_server.model_routes` alias matching `model`, the direct request `model`/`provider`, then the profile default; on Chat Completions and Responses a bare `model` without `provider` SHALL be ignored unless `api_server.direct_model_requests = true` (default false) so generic clients sending `gpt-4o` keep the profile default, a `provider` conflicting with a matched route alias SHALL answer 400, an unknown provider SHALL answer 400 naming it, and a per-request model SHALL change only the model call while the session's system prompt and tool list stay byte-identical.
**Milestone:** M2

#### Scenario: Generic client model ignored
- **GIVEN** `direct_model_requests = false`
- **WHEN** Open WebUI sends `"model":"gpt-4o"` without `provider`
- **THEN** the turn SHALL run on the profile's default model and `runtime.route_source` SHALL be `global`

#### Scenario: Route alias conflict
- **GIVEN** route alias `fast` mapped to provider `groq`
- **WHEN** a request sends `model = "fast"` and `provider = "openai"`
- **THEN** the system SHALL answer 400 and run nothing

### Requirement: Concurrent run cap
Each profile SHALL limit simultaneously executing runs started by the Chat Completions, Responses, Runs and session-chat endpoints to `api_server.max_concurrent_runs` (default 10; 0 disables; negative fails validation), answering further run-starting requests with HTTP 429 `Too many concurrent runs (max <n>)` and `Retry-After`, while runs triggered through the Jobs API are governed by scheduled-jobs limits; a run counts until its executor work actually exits, including while `stopping`.
**Milestone:** M2

#### Scenario: Eleventh run refused
- **GIVEN** 10 running API runs
- **WHEN** an eleventh `POST /v1/runs` arrives
- **THEN** the system SHALL answer 429 `Too many concurrent runs (max 10)`

### Requirement: Sessions REST API
The API server SHALL expose `GET /api/sessions` (`limit` default 50 max 200, `offset`, `source`, `include_children`), `POST /api/sessions` (create an empty API session), `GET`, `PATCH` (title, `end_reason`) and `DELETE /api/sessions/{id}`, `GET /api/sessions/{id}/messages` (`include_compacted` default false, `inline_images` default true; false renders `[image]` placeholders), `POST /api/sessions/{id}/fork` (lineage kind `fork`), `POST /api/sessions/{id}/chat` (one synchronous turn returning `status` and the reply) and `POST /api/sessions/{id}/chat/stream` (SSE `assistant.delta`, `assistant.commentary`, `tool.started`, `tool.completed`, `tool.failed` and one terminal `run.completed`, `run.failed` or `run.cancelled`); reads SHALL cover every session of the profile, while chat, PATCH and DELETE on a session owned by another surface SHALL answer 409 `permission_denied`.
**Milestone:** M2

#### Scenario: Fork and continue
- **WHEN** a client calls `POST /api/sessions/ses_A/fork` then `POST /api/sessions/<new>/chat`
- **THEN** the new session SHALL have `parent_session_id = ses_A` with kind `fork` and the turn SHALL see `ses_A`'s history

#### Scenario: Light transcript
- **WHEN** a client requests messages with `inline_images=false`
- **THEN** image attachments SHALL be rendered as `[image]` placeholders

### Requirement: Jobs REST API
The API server SHALL expose the profile's scheduled jobs as `GET /api/jobs`, `POST /api/jobs`, `GET`, `PATCH` and `DELETE /api/jobs/{id}`, and `POST /api/jobs/{id}/pause`, `/resume` and `/run`, with request and response fields equal to the `cronjob` tool schema and the same validation, confirmation and error messages as `prometheus cron`; a job created over the API SHALL record `origin` `api`, a validation error SHALL answer 400 with the scheduled-jobs message, and an unknown id 404.
**Milestone:** M2

#### Scenario: Past one-shot resume
- **GIVEN** a paused `once` job whose instant has passed
- **WHEN** a client posts `/api/jobs/{id}/resume`
- **THEN** the system SHALL answer 400 with "cannot resume: one-shot time <t> is in the past; use 'prometheus cron edit --schedule'"

### Requirement: Discovery and readiness endpoints
The API server SHALL expose `GET /v1/capabilities` (`object: "prometheus.api_server.capabilities"`, model id, `auth {type: "bearer", required: true}`, a `features` map with `chat_completions`, `responses_api`, `run_submission`, `run_status`, `run_events_sse`, `run_stop`, `run_approval`, `run_clarify`, `run_unknown_resolution`, `reasoning_streaming`, `tool_progress_events`, `sessions_api`, `jobs_api`, and `session_key_header`), `GET /api/model/options` (configured providers and models with capability hints and pricing), `GET /v1/skills` (name, description, category), `GET /v1/toolsets` (name, label, description, enabled, configured, tools) and authenticated `GET /health/detailed` reporting bounded status and counts for config validity, `state.db`, configured model, free disk, adapter states, active API runs and active delegations — never config values, credentials, paths, commands or raw errors — with HTTP 200 and a top-level `status` of `ok` or `degraded`.
**Milestone:** M2

#### Scenario: Readiness hides secrets
- **GIVEN** a provider credential that fails validation
- **WHEN** `GET /health/detailed` is called
- **THEN** the response SHALL report the model check as failed with a stable code and SHALL contain no key material or file paths

### Requirement: Gateway proxy mode
A profile MAY set `gateway.proxy.url` (an API server base URL) and `gateway.proxy.key_secret`, in which case its messaging adapters SHALL keep authorization, pairing, routing keys and delivery locally but SHALL forward every admitted turn to the remote API server as a session-chat request keyed by the local conversation key (`X-Prometheus-Session-Key`), stream the reply back to the chat, and run no local agent for that profile; a remote 401 SHALL put the profile's adapters in `fatal` with code `proxy_auth`, and an unreachable remote SHALL reply "Upstream agent unavailable, try again shortly." without queueing a local turn.
**Milestone:** M3

#### Scenario: Container relays to host agent
- **GIVEN** a containerized daemon with Matrix enabled and `gateway.proxy.url = "http://host.docker.internal:8642/p/default"`
- **WHEN** a Matrix user sends a message
- **THEN** the reply SHALL be produced by the host's agent and the container SHALL record no local turn

### Requirement: Subscription proxy
`prometheus gateway proxy start [--provider <id>] [--host 127.0.0.1] [--port 8645] [--allow-lan]` SHALL run a local OpenAI-compatible passthrough (no agent loop, no tools) for a provider whose OAuth or token-mint login the profile holds (built-ins `nous`, default, and `xai`), forwarding only that provider's allowed paths (for `nous`: `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/models`) verbatim with streaming preserved and answering any other path with 404 listing the allowed paths; it SHALL ignore the client's `Authorization` header and attach the real credential, refreshed under the cross-process refresh lock, SHALL never log request or response bodies, SHALL record each call in the usage ledger with `billing_source = "subscription:<provider>"` and surface `proxy`, SHALL refuse requests whose `Host` is not its own address and any browser-originated request (foreign `Origin`, or `Sec-Fetch-Site` other than `none`), and SHALL refuse a non-loopback `--host` unless `--allow-lan` is given, in which case clients MUST present the bearer in secret `PMT_PROXY_CLIENT_KEY` (generated on first use) (upgrade over Hermes' unauthenticated LAN mode); `prometheus gateway proxy status` SHALL print per-provider readiness (`ready (expires <time>)`, `not logged in`, `credentials need attention`) and `prometheus gateway proxy providers` the available providers, and `start` SHALL exit 78 when the provider is not logged in.
**Milestone:** M3

#### Scenario: Disallowed path
- **GIVEN** the proxy running for `nous`
- **WHEN** a client calls `POST /v1/images/generations`
- **THEN** the system SHALL answer 404 listing the four allowed paths and send nothing upstream

#### Scenario: Client bearer replaced
- **WHEN** a client sends `Authorization: Bearer sk-unused` to `/v1/chat/completions`
- **THEN** the upstream request SHALL carry the profile's current Nous credential and the usage ledger SHALL record `subscription:nous`

#### Scenario: LAN requires a client key
- **GIVEN** `prometheus gateway proxy start --host 0.0.0.0 --allow-lan`
- **WHEN** a LAN client calls without the proxy client key
- **THEN** the system SHALL answer 401

# Mixture of Agents

## Purpose
Mixture of Agents (MoA) lets one model call draw on several models: named presets run a set of reference (advisor) models over the conversation and hand their outputs to an aggregator model that acts with the normal tool surface. The key decision is that MoA is a virtual provider `moa` whose models are presets, so it is selected, resolved, cached, billed and used as a fallback entry through the ordinary `model-providers` machinery, never through a toolset, and it never breaks the conversation's cached prompt prefix.

## ADDED Requirements

### Requirement: MoA virtual provider and presets
The system SHALL register a built-in virtual provider `moa` (alias `mixture-of-agents`, virtual endpoint `moa://local`, auth `none`) whose models are the presets under `[moa.presets.<name>]`, each preset requiring `reference_models` (a non-empty array of `{provider, model, reasoning_effort?}`) and `aggregator` (`{provider, model, reasoning_effort?}`) and optionally `reference_temperature`, `aggregator_temperature`, `fanout` and `enabled` (default true), and SHALL ship a default preset `default` with references `openai-codex/gpt-5.5` and `openrouter/deepseek/deepseek-v4-pro` and aggregator `openrouter/anthropic/claude-opus-4.8`, selected by `moa.default_preset` (default `"default"`).
**Milestone:** M2

#### Scenario: Preset selectable as a model
- **GIVEN** a preset `review` under `[moa.presets.review]`
- **WHEN** the user runs `/model review --provider moa`
- **THEN** the session's route SHALL become provider `moa`, model `review`

#### Scenario: Bare preset name
- **GIVEN** a preset `review` and no other model or alias named `review`
- **WHEN** the user runs `/model review`
- **THEN** the system SHALL select `moa/review`

#### Scenario: Unknown preset
- **WHEN** a session requests `moa/nope`
- **THEN** the system SHALL fail with `model.unknown` listing the configured preset names

### Requirement: Preset validation
The system SHALL validate presets when the config loads, failing with exit 78 and the offending key when a slot names an unknown provider, when `aggregator.provider = "moa"` (recursive MoA is not allowed), when `reasoning_effort` is not one of `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`, or when a temperature is outside 0.0-2.0, and SHALL never send a temperature for a slot whose temperature key is omitted.
**Milestone:** M2

#### Scenario: Recursive aggregator refused
- **GIVEN** `[moa.presets.a] aggregator = {provider = "moa", model = "b"}`
- **WHEN** the config is validated
- **THEN** the system SHALL fail naming `moa.presets.a.aggregator`

#### Scenario: Temperature omitted by default
- **GIVEN** a preset without `reference_temperature`
- **WHEN** a reference call is sent
- **THEN** the request SHALL carry no temperature field

### Requirement: MoA call procedure
The system SHALL, for each main model call routed to a MoA preset whose `enabled = true`, run every reference model concurrently without tool schemas on a deterministic view of the conversation containing only user and assistant text (no system prompt, no tool calls or tool results), then call the aggregator with the session's normal system prompt, tool schemas and history plus the reference outputs, and treat the aggregator's response (text and tool calls) as the model response that the turn loop executes; a preset with `enabled = false` SHALL call the aggregator alone exactly as a plain model.
**Milestone:** M2

#### Scenario: References never see tools
- **GIVEN** a session on a MoA preset with two references and a 12-tool surface
- **WHEN** a main call runs
- **THEN** each reference request SHALL contain no tool schema and no system prompt, and the aggregator request SHALL contain all 12 tool schemas

#### Scenario: Aggregator tool call executed
- **GIVEN** the aggregator returns a `read_file` tool call
- **WHEN** the turn loop handles the response
- **THEN** the system SHALL execute `read_file` normally and the next iteration SHALL follow the preset's `fanout`

### Requirement: Cache-safe guidance injection
The system SHALL append the reference outputs to the aggregator request as one trailing private user message of labelled guidance placed after the entire stable prefix (system prompt, tools, history including the current user message) and never merged into the user's own message text, SHALL never persist that guidance block into the session history replayed on later iterations, and on wires that require merged adjacent user turns (`anthropic_messages`, `bedrock_converse`, `gemini_native`) SHALL send it as a separate content block of the same user message so the user's block stays byte-identical across iterations.
**Milestone:** M2

#### Scenario: Prefix preserved across iterations
- **GIVEN** a MoA turn with two tool iterations
- **WHEN** the second aggregator request is built
- **THEN** its bytes up to the previous iteration's guidance position SHALL equal the previous request's prefix with the old guidance block removed

### Requirement: Strict-alternation recovery
The system SHALL, when an aggregator destination rejects consecutive user messages with HTTP 400 (for example `Conversation roles must alternate`), retry that request once with the user message and guidance merged, remember the destination (base URL and model) as merge-required for the rest of the session, merge up front for that destination on later requests, and keep every other destination on the split shape.
**Milestone:** M2

#### Scenario: Merge remembered per destination
- **GIVEN** an aggregator on a vLLM endpoint whose chat template enforces alternation
- **WHEN** the first MoA request returns HTTP 400 for role alternation
- **THEN** the system SHALL retry merged, succeed, and send merged requests to that endpoint for the remaining turns without another 400

### Requirement: Advisor cadence
The system SHALL run references according to the preset's `fanout`: `user_turn` (default) on the first model iteration of each user turn only, reusing that guidance for the turn's later iterations; `per_iteration` on every iteration; and `every_n:N` (N >= 2, also accepted as `{mode = "every_n", n = N}` and normalised) on the turn's iterations i (1-based) where i = 1 or i is a multiple of N, reusing the most recent guidance in between, with the counter reset on each new user message; an unknown or malformed value SHALL log one WARN and behave as `user_turn`.
**Milestone:** M2

#### Scenario: Every third iteration
- **GIVEN** `fanout = "every_n:3"` and a turn with 7 model iterations
- **WHEN** the turn completes
- **THEN** references SHALL have run on iterations 1, 3 and 6 and every iteration's aggregator request SHALL carry guidance

#### Scenario: Malformed fanout
- **GIVEN** `fanout = "sometimes"`
- **WHEN** the preset is used
- **THEN** references SHALL run once per user turn and a WARN SHALL name the preset

### Requirement: Reference failures are non-fatal
The system SHALL, when a reference call fails (credential, rate limit, timeout, error class from `model-providers`), include a one-line failure note for that reference in the guidance and continue with the references that succeeded, calling the aggregator even when every reference failed; only an aggregator failure SHALL fail the model call (and then follow the session's fallback chain).
**Milestone:** M2

#### Scenario: One advisor missing credentials
- **GIVEN** a preset whose first reference has no credential
- **WHEN** a main call runs
- **THEN** the aggregator SHALL receive the second reference's output plus a note that the first was unavailable, and the turn SHALL complete

### Requirement: Privacy filter for advisor outputs
The system SHALL apply `moa.privacy_filter` (`""` off by default, `display`, `full`) to advisor text: `display` SHALL redact known secrets, email addresses and delimited phone numbers (for example `(555) 123-4567`, `555-123-4567`) in the reference blocks shown to users and in saved traces while the aggregator receives raw text; `full` SHALL additionally redact the guidance injected into the aggregator and the `/moa` synthesis input; bare digit runs, line numbers, timestamps, git SHAs and IP addresses SHALL never be redacted by this filter.
**Milestone:** M2

#### Scenario: Display-only redaction
- **GIVEN** `moa.privacy_filter = "display"` and an advisor output containing `alice@example.com`
- **WHEN** the reference block is rendered and the aggregator request is built
- **THEN** the rendered block SHALL show the address redacted and the aggregator request SHALL contain it unredacted

#### Scenario: SHA untouched
- **GIVEN** `moa.privacy_filter = "full"` and advisor output containing commit `a1b2c3d4e5f6`
- **WHEN** guidance is injected
- **THEN** the SHA SHALL appear unchanged

### Requirement: MoA traces
The system SHALL, when `moa.save_traces = true` (default false), append one JSONL record per MoA model call to `<profile>/moa-traces/<session_id>.jsonl` (or under `moa.trace_dir` when set) containing each reference's and the aggregator's exact request summary, output, usage and error, after applying the privacy filter and known-secret redaction, and SHALL include these files in session export and retention.
**Milestone:** M2

#### Scenario: Trace written
- **GIVEN** `moa.save_traces = true`
- **WHEN** a MoA turn with two references completes
- **THEN** the session's trace file SHALL contain one record listing two reference entries and one aggregator entry with usage

### Requirement: MoA billing and accounting
The system SHALL record every reference and aggregator call as its own usage-ledger row under the owning session with `purpose = "moa.reference"` or `purpose = "moa.aggregator"`, the preset name as `purpose_detail` and the slot's own `billing_source` (ledger columns per `observability` › Per-call usage ledger), SHALL label the session's billing source in status surfaces as the aggregator's, and SHALL print the notice `Aggregator is on <provider>; the whole tool loop will be billed there, not to <model.provider>.` in `prometheus moa list`, `prometheus moa configure` and when a preset is selected, whenever the aggregator's provider differs from `model.provider`.
**Milestone:** M2

#### Scenario: Ledger rows per slot
- **GIVEN** a MoA call with two references and one aggregator
- **WHEN** `prometheus usage --session <id> --by model --json` runs
- **THEN** the output SHALL include three rows' worth of calls attributed to their own providers and models

### Requirement: MoA as a fallback entry
The system SHALL accept `{provider = "moa", model = "<preset>"}` in `[[fallback.chain]]`, activating the whole preset (references and aggregator) when reached, and SHALL skip the entry when the preset does not resolve or its aggregator has no usable credential.
**Milestone:** M2

#### Scenario: Preset activated on primary failure
- **GIVEN** a chain `[moa/default]` and a primary that fails with `overloaded`
- **WHEN** fallback activates
- **THEN** the next call SHALL run the `default` preset's references and aggregator and `provider.fallback` SHALL name `moa/default`

### Requirement: One-shot /moa command
The system SHALL provide `/moa <prompt>` on every chat surface, running exactly one user turn with the whole argument as the literal prompt on the default preset and restoring the previous route at the end of that turn (including on interrupt or error), SHALL print usage for a bare `/moa`, and SHALL never interpret the argument as a preset name or switch the session's model persistently.
**Milestone:** M2

#### Scenario: Route restored after one turn
- **GIVEN** a session on `anthropic/claude-x`
- **WHEN** the user runs `/moa design a migration plan` and then sends another message
- **THEN** the first turn SHALL run on `moa/default` and the following turn SHALL run on `anthropic/claude-x`

### Requirement: Preset management commands
The system SHALL provide `prometheus moa list [--json]` (marking the aggregator as the acting, billed model and listing references with their cadence), `prometheus moa configure [<name>]` (interactive create or update, default `moa.default_preset`, using the model picker for each slot and validating credentials), `prometheus moa delete <name>` (refusing to delete the preset named by `moa.default_preset` unless another default is set first, exit 1), and RPC methods `moa.presets.list`, `moa.presets.put` and `moa.presets.delete` used by the dashboard and desktop preset editors; preset edits SHALL apply to new model calls only, never altering a request already in flight.
**Milestone:** M2

#### Scenario: Deleting the default preset refused
- **GIVEN** `moa.default_preset = "default"`
- **WHEN** the user runs `prometheus moa delete default`
- **THEN** the system SHALL exit with status 1 naming `moa.default_preset`

#### Scenario: Presets in every picker
- **GIVEN** two configured presets
- **WHEN** the model picker opens in the CLI, TUI, dashboard or desktop
- **THEN** a `Mixture of Agents` provider row SHALL list both presets as models

### Requirement: MoA preserves the session prompt cache
Selecting, switching to or away from a MoA preset SHALL be an ordinary route change under `model-providers` that never modifies the system prompt, tool schemas or prior messages, and the system SHALL apply the same prompt-cache key rules to reference and aggregator calls as to any call on their providers.
**Milestone:** M2

#### Scenario: Returning from MoA replays the prefix
- **GIVEN** a session that ran turn 1 on `anthropic/claude-x`, turn 2 on `moa/default`, and switches back
- **WHEN** turn 3 is sent to `anthropic/claude-x`
- **THEN** the system prompt and tool schema bytes SHALL be identical to turn 1's

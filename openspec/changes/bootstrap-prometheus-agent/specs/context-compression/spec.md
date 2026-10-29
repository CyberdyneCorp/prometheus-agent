# Context Compression

## Purpose
Context compression keeps a long session inside the main model's context window by pruning old tool output deterministically and replacing the middle of the transcript with a structured handoff summary written by the auxiliary compression model. The key decision is that compaction is the ONLY sanctioned rewrite of already-sent context and the only automatic point where the system prompt is rebuilt mid-session; it always happens in place on the same session id, keeps the originals recoverable, and must pass deterministic quality checks before it commits.

## ADDED Requirements

### Requirement: Context engine selection
The system SHALL select the context engine from `context.engine` (default `"builtin"`); any other value SHALL name a context engine registered by an installed and enabled plugin, and an unknown name SHALL fail config validation with `unknown context engine '<name>'` and exit code 78 instead of silently falling back.
**Milestone:** M1

#### Scenario: Unknown engine
- **GIVEN** `context.engine = "lcm"` with no plugin registering `lcm`
- **WHEN** `prometheus chat` starts
- **THEN** the system SHALL print `unknown context engine 'lcm'` and exit 78

#### Scenario: Plugin engine not auto-activated
- **GIVEN** a plugin that registers a context engine and no `context.engine` key
- **WHEN** a session starts
- **THEN** the built-in engine SHALL be used

### Requirement: Engine output is validated by the host
The system SHALL validate every compaction result, from any engine, before committing it: strict role alternation, every tool result preceded by its tool call, no tool call without a result except on the trailing in-flight assistant row, fewer estimated tokens than the input, and an intact current-turn user row; an invalid result SHALL be discarded and recorded as a summary failure with reason `invalid_engine_output`.
**Milestone:** M2

#### Scenario: Plugin engine breaks alternation
- **GIVEN** a plugin engine that returns two adjacent assistant rows
- **WHEN** the host validates the result
- **THEN** the transcript SHALL stay unchanged and `compression.failed` SHALL carry reason `invalid_engine_output`

### Requirement: Compaction is the only sanctioned cache break
The system SHALL rewrite already-sent context only through a committed compaction, and at commit it SHALL rebuild the system prompt from current disk state (memory snapshot, skills index, plugin sections) and refresh tool schemas, persist the new prompt bytes and hash on the session row, and emit `prompt.rebuilt` with reason `compaction`; when the rebuilt bytes equal the stored bytes the stored prompt SHALL be kept unchanged.
**Milestone:** M1

#### Scenario: Memory grew during the session
- **GIVEN** memory entries added since the session started
- **WHEN** a compaction commits
- **THEN** the next request's system prompt SHALL include the updated memory snapshot

### Requirement: Compression configuration schema
The `compression` section SHALL default to `enabled = true`, `threshold = 0.50`, `threshold_tokens = 0` (off), `target_ratio = 0.20`, `tail_mode = "lean"`, `protect_first_n = 3`, `min_tail_user_messages = 1`, `max_attempts = 3`, `summary_max_tokens = 10000`, `abort_on_summary_failure = false`, `quality_min_coverage = 0.8`, `pinned_max_tokens = 8000`, `micro_compact = false`, `micro_compact_every_n_turns = 1`, `proactive_prune_tokens = 0`, `prepare_ahead = false`, and `summary_timeout_secs = 120`; out-of-range values (`threshold` outside (0, 1), `target_ratio` outside [0.10, 0.80], `max_attempts` outside 1..10) and unknown enum values SHALL fail validation with exit 78 rather than being clamped.
**Milestone:** M1

#### Scenario: Out-of-range attempts
- **GIVEN** `compression.max_attempts = 50`
- **WHEN** config is validated
- **THEN** the system SHALL report `compression.max_attempts: must be between 1 and 10` and exit 78

#### Scenario: Unknown tail mode
- **GIVEN** `compression.tail_mode = "fat"`
- **WHEN** config is validated
- **THEN** the system SHALL report the allowed values `lean`, `ratio` and exit 78

### Requirement: Threshold from the main model's input budget
The system SHALL compute the trigger as `threshold_tokens = floor((context_window - reserved_output_tokens) * effective_threshold)` using the MAIN model's window, floored at 64,000 tokens but never above 85% of the input budget, further lowered by a non-zero `compression.threshold_tokens` and by the compression model's own input window when that is smaller, and SHALL recompute it on every model switch.
**Milestone:** M1

#### Scenario: Absolute cap
- **GIVEN** a 1,000,000-token window, `threshold = 0.50` and `threshold_tokens = 200000`
- **WHEN** the threshold is resolved
- **THEN** it SHALL be 200,000 tokens

#### Scenario: Output reservation
- **GIVEN** a 200,000-token window, 65,536 reserved output tokens and an effective threshold of 0.80
- **WHEN** the threshold is resolved
- **THEN** it SHALL be `floor(134464 * 0.80)`

### Requirement: Small-window threshold floor
For a main model whose context window is below 512,000 tokens the effective threshold SHALL be at least 0.75 (a higher configured value wins), and the floor SHALL be re-derived from the configured value on every model switch in both directions.
**Milestone:** M1

#### Scenario: Switch from 200K to 1M
- **GIVEN** `threshold = 0.50` and a session switching from a 200,000-token model to a 1,000,000-token model
- **WHEN** the switch completes
- **THEN** the effective threshold SHALL go from 0.75 to 0.50

### Requirement: Per-model threshold overrides
`compression.model_thresholds` SHALL map a model-name substring, or `"<provider>:<substring>"`, to a threshold, the longest matching substring winning and a provider-qualified key beating a bare key with the same substring; the override SHALL be re-resolved on every model switch with the small-window floor applied on top.
**Milestone:** M2

#### Scenario: Longest key wins
- **GIVEN** `model_thresholds = { "glm-5" = 0.40, "glm-5-1m" = 0.25 }`
- **WHEN** the model is `glm-5-1m`
- **THEN** the base threshold SHALL be 0.25

### Requirement: Trigger decision and blocking reasons
The system SHALL decide compaction with `should_compress(tokens)` returning `run`, `below_threshold`, or `blocked(<reason>)` where reason is `cooldown:<seconds remaining>`, `backoff:<seconds remaining>` or `ineffective`; the `ineffective` breaker SHALL trip after two consecutive compactions that the provider's next reported usage shows reclaimed less than 10% of tokens, and cooldown and breaker state SHALL be persisted in `state.db` so restarts honour them.
**Milestone:** M1

#### Scenario: Cooldown armed
- **GIVEN** tokens over threshold and a summary-failure cooldown with 120 s remaining
- **WHEN** `should_compress` is evaluated
- **THEN** it SHALL return `blocked(cooldown:120)`

### Requirement: Provider usage beats rough estimates
The system SHALL prefer the provider-reported input token count of the latest request over a local estimate, and when only a local estimate is over threshold and the provider has not been observed to omit usage it SHALL send one request uncompressed and decide on the reported usage; a provider known to omit usage SHALL be judged on the estimate.
**Milestone:** M1

#### Scenario: Resumed session first request
- **GIVEN** a resumed session whose rough estimate is over threshold and no reported usage yet
- **WHEN** the pre-request gate runs
- **THEN** the system SHALL send the request uncompressed and evaluate compaction on its reported usage

### Requirement: Trigger points and attempt budget
When `compression.enabled` is true the turn loop SHALL evaluate compaction at turn start, before each model request, and after each tool round, each attempt counting against `compression.max_attempts` per turn and carrying a trigger label from `manual`, `turn_start`, `pre_request`, `post_tool`, `overflow`, `gateway_hygiene`; an attempt that does not reach the provider SHALL not consume an iteration.
**Milestone:** M1

#### Scenario: Tool output pushes over threshold
- **GIVEN** a tool round whose results push the next request over threshold with attempts remaining
- **WHEN** the post-tool check runs
- **THEN** the system SHALL compact with trigger `post_tool` before the next request

#### Scenario: Attempts exhausted
- **GIVEN** `max_attempts` already used this turn and the request still over threshold
- **WHEN** the next request is prepared
- **THEN** the system SHALL not start another compaction in that turn

### Requirement: Context overflow recovery
When a provider rejects a request as exceeding the context window, the system SHALL compact once with trigger `overflow`, ignoring an armed cooldown for that single attempt without clearing it, and retry the request; if the request still overflows after the per-turn attempts are spent the turn SHALL end with exit reason `context_exhausted` and the message "Context is full. Run /compress here 2 to keep only the last exchanges, or /new to start a new session."
**Milestone:** M1

#### Scenario: Huge pasted file
- **GIVEN** a user message that makes the request exceed the window
- **WHEN** the provider returns a context-length error
- **THEN** the system SHALL compact with trigger `overflow` and retry the request

### Requirement: One compaction at a time per session
The system SHALL hold a per-session compaction lock in `state.db` for the whole compaction, and a second attempt on the same session while the lock is held SHALL return status `lock_skipped` without changing the transcript and without consuming an attempt.
**Milestone:** M1

#### Scenario: Gateway hygiene races a manual compress
- **GIVEN** gateway hygiene compacting session S
- **WHEN** the user runs `/compress` on S
- **THEN** the command SHALL report `lock_skipped`

### Requirement: Gateway session hygiene
Before the gateway hands an inbound message to the agent, it SHALL compact the session with trigger `gateway_hygiene` when the session's last known input tokens reach 85% of the main model's window or the message count reaches 5,000, using the last provider-reported count, then the persisted usage anchor, then a rough estimate, and SHALL skip sessions with fewer than 4 messages or with compression disabled.
**Milestone:** M2

#### Scenario: Overnight growth
- **GIVEN** a Telegram session at 90% of the window
- **WHEN** a new message arrives
- **THEN** the gateway SHALL compact before the agent runs the turn

### Requirement: Deterministic recoverable prune
The first compaction phase SHALL run without any model call on a copy of the transcript: identical tool results SHALL be deduplicated to "[duplicate tool output, same as a later call]", tool results over 200 characters outside the protected tail SHALL be replaced by one line `[pruned tool result] <tool>(<arguments digest, max 120 chars>) -> <N> chars, ref <12 hex chars>` whose original content is kept in the blob store under that ref, stale image payloads SHALL be replaced by `[image removed, ref <ref>]`, and tool-call arguments, the unanswered tool round and skill bodies loaded this turn SHALL never be pruned.
**Milestone:** M1

#### Scenario: Large old output
- **GIVEN** a 40,000-character `terminal` result ten exchanges back
- **WHEN** the prune runs
- **THEN** the row SHALL read `[pruned tool result] terminal(...) -> 40000 chars, ref <ref>` and the original SHALL be retrievable from the blob store by that ref

### Requirement: Pruned content is retrievable by the agent (upgrade)
The system SHALL let the agent fetch a pruned or summarized tool result by its ref through the existing `session_search` tool (argument `ref`), returning the original content subject to the normal tool-result size budget, so compaction never makes an earlier result irrecoverable within the session.
**Milestone:** M2

#### Scenario: Agent needs an old log
- **GIVEN** a pruned `terminal` result with ref `a1b2c3d4e5f6`
- **WHEN** the agent calls `session_search` with `ref = "a1b2c3d4e5f6"`
- **THEN** the tool SHALL return the original output

### Requirement: Protected head
The system message SHALL always be protected and is not counted; `compression.protect_first_n` (default 3) SHALL count the non-system messages after it, and SHALL apply only to a session's first compaction: once a session has a compaction summary, only the system message is head-protected.
**Milestone:** M1

#### Scenario: Second compaction
- **GIVEN** a session compacted once with `protect_first_n = 3`
- **WHEN** it compacts again
- **THEN** only the system message SHALL be head-protected

### Requirement: Token-budgeted tail
The protected tail SHALL be chosen by walking backward under a token budget, in `lean` mode 2.5% of the main window clamped to [10,000, 25,000] tokens and in `ratio` mode `threshold_tokens * target_ratio`, never above 20% of the window; it SHALL include at least `min_tail_user_messages` real user messages even past the budget, and its boundary SHALL move earlier so an assistant tool call and its results are never split.
**Milestone:** M1

#### Scenario: Small local model
- **GIVEN** a 16,000-token model in `lean` mode
- **WHEN** the tail budget is computed
- **THEN** it SHALL be at most 3,200 tokens

#### Scenario: Tool group at the cut
- **GIVEN** a cut between an assistant tool call and its results
- **WHEN** boundaries are aligned
- **THEN** the cut SHALL move before the assistant row

### Requirement: Orphan handling stubs missing results
After assembly the system SHALL drop any tool result whose tool call is no longer present and SHALL give any tool call (other than those on the trailing in-flight assistant row) whose result is no longer present a stub result "[result folded into the context summary]", keeping the call itself so the record of the action survives; this SHALL be the only documented orphan behavior.
**Milestone:** M1

#### Scenario: Result fell into the summary
- **GIVEN** an assistant tool call in the tail whose result was summarized
- **WHEN** the compacted transcript is assembled
- **THEN** the call SHALL remain and be followed by the stub result

### Requirement: Summary budget
The summary budget SHALL be 20% of the estimated tokens of the region being summarized, at least 2,000 tokens and at most `min(5% of the main window, compression.summary_max_tokens)` (default 10,000), and the summary request SHALL not set a provider output cap below that budget.
**Milestone:** M1

#### Scenario: 100K middle on a 200K model
- **GIVEN** a 200,000-token model and a 100,000-token middle region
- **WHEN** the summary budget is computed
- **THEN** it SHALL be 10,000 tokens

### Requirement: Handoff summary shape
The summary row SHALL begin with `[CONTEXT COMPACTION - REFERENCE ONLY]`, contain the sections `## Goal`, `## Progress`, `## Decisions`, `## Files`, `## Open Todos`, `## Pinned` (when any) and `## Next Steps`, end with `--- END OF CONTEXT SUMMARY - respond to the message below, not the summary above ---`, carry the metadata flag `compaction_summary` (never sent on the wire), and be placed with a role (or merged into the first tail row) that keeps strict role alternation.
**Milestone:** M1

#### Scenario: Tail starts with a user message
- **GIVEN** a compaction whose tail begins with a user row
- **WHEN** the transcript is assembled
- **THEN** no two adjacent rows SHALL share a role

### Requirement: Iterative summary update
On a session's later compactions the system SHALL pass the previous summary to the summarizer as the document to update rather than summarizing from scratch, and a deterministic fallback summary SHALL also become the previous summary.
**Milestone:** M1

#### Scenario: Second compaction
- **GIVEN** a session compacted once with summary S1
- **WHEN** it compacts again
- **THEN** the summarizer request SHALL include S1 as the summary to update

### Requirement: Summary quality check (upgrade)
Before committing, the system SHALL extract deterministically from the summarized region the set of file paths written or patched, the open todo items from the `todo` tool state, the latest user request text, and tool errors not followed by a success of the same tool, and compute coverage as the share of those items present in the summary; below `compression.quality_min_coverage` it SHALL retry the summary once with the missing items listed, and in all cases it SHALL append any still-missing items verbatim under `## Preserved References`, recording the coverage in `compression.committed`.
**Milestone:** M2

#### Scenario: Summary forgot a file
- **GIVEN** a summarized region that patched `src/auth.rs` and a summary not mentioning it
- **WHEN** the quality check runs
- **THEN** the committed summary SHALL list `src/auth.rs` under `## Preserved References` or in its body

#### Scenario: Open todos survive verbatim
- **GIVEN** a todo list with two items in status `pending`
- **WHEN** a compaction commits
- **THEN** the `## Open Todos` section SHALL contain both items exactly as stored by the `todo` tool

### Requirement: Pinned messages survive compaction (upgrade)
The system SHALL let the user pin a message with `/pin [<message-id>|last]` and unpin it with `/unpin <message-id>`, SHALL copy pinned messages verbatim into the summary's `## Pinned` section on every compaction, and SHALL refuse a pin that would make pinned content exceed `compression.pinned_max_tokens` with `pinned content would exceed <N> tokens; unpin something first`.
**Milestone:** M2

#### Scenario: Pinned requirement kept
- **GIVEN** a pinned user message "never touch the prod database"
- **WHEN** three compactions run
- **THEN** that exact text SHALL appear in the latest summary's `## Pinned` section

### Requirement: Summary failure handling
A failed, timed-out or truncated summary SHALL arm a persisted per-session cooldown escalating 60 s, 300 s, 900 s, and when `abort_on_summary_failure` is false the system SHALL commit a deterministic fallback summary (built from the prune lines, the quality-check items and the previous summary) except for auth and quota errors, which SHALL abort with the transcript unchanged; a manual `/compress` SHALL clear the cooldown and run.
**Milestone:** M1

#### Scenario: Three timeouts
- **GIVEN** three consecutive summary timeouts in one session
- **WHEN** each is recorded
- **THEN** the cooldowns SHALL be 60 s, 300 s and 900 s

#### Scenario: Manual retry
- **GIVEN** an armed cooldown
- **WHEN** the user runs `/compress`
- **THEN** the cooldown SHALL be cleared and a summary attempt made

### Requirement: Compaction is always in place
A committed compaction SHALL keep the same session id, mark the replaced rows `archived` with the compaction's epoch number so they remain readable, searchable and exportable, and emit `compression.committed` with `trigger`, `epoch`, `tokens_before`, `tokens_after`, `summary_tokens`, `coverage` and `duration_ms`; there SHALL be no configuration that rotates to a new session id on compaction.
**Milestone:** M1

#### Scenario: Default compaction
- **GIVEN** session S
- **WHEN** a compaction commits
- **THEN** the agent SHALL still be on session S and `session_search` SHALL find text from the archived rows

### Requirement: Manual compaction command
Every surface SHALL run manual compaction through one parser for `/compress [--preview] [here [N]] [<focus text>]` (default `N = 2` for `here`), returning status `preview` (estimated before/after tokens and the rows that would be summarized, transcript untouched), `nothing_to_do`, `lock_skipped` or `compressed`; `here N` SHALL summarize everything except the last N user exchanges, and focus text SHALL be passed to the summarizer as a priority instruction.
**Milestone:** M1

#### Scenario: Preview
- **GIVEN** `/compress --preview`
- **WHEN** it runs
- **THEN** the status SHALL be `preview` and the stored transcript SHALL be unchanged

#### Scenario: Keep last two exchanges
- **GIVEN** a session with 30 user exchanges
- **WHEN** the user runs `/compress here`
- **THEN** the tail SHALL hold the last 2 user exchanges verbatim and all earlier rows SHALL be summarized

### Requirement: Opt-in micro-compaction
When `compression.micro_compact` is true, after every `micro_compact_every_n_turns` completed turns (not interrupted or failed) the system SHALL fold exactly one assistant exchange from the compressible middle into a single rolling summary row, SHALL never fold user-authored messages, the protected head or the protected tail, SHALL keep only the newest rolling row, and SHALL skip an exchange after 3 consecutive failures on it; a micro-compaction error SHALL be logged and SHALL not fail the turn.
**Milestone:** M2

#### Scenario: Off by default
- **GIVEN** default config
- **WHEN** a turn completes
- **THEN** no micro-compaction SHALL run

#### Scenario: User messages preserved
- **GIVEN** micro-compaction enabled over a long session
- **WHEN** many passes run
- **THEN** every original user message SHALL remain verbatim

### Requirement: Prepared-ahead summaries
When `compression.prepare_ahead` is true and input tokens pass 90% of `threshold_tokens`, the system SHALL generate the summary in the background against the current transcript and, at the trigger, adopt it only if the summarized region is unchanged and append-only extension is the sole difference, otherwise discard it and summarize normally.
**Milestone:** M2

#### Scenario: Prepared summary adopted
- **GIVEN** a summary prepared at 92% of threshold and only new tail rows since
- **WHEN** the threshold is crossed
- **THEN** the compaction SHALL commit without a new summarizer call

### Requirement: Opt-in proactive prune
When `compression.proactive_prune_tokens` is above 0 and input tokens pass it, the system SHALL run only the deterministic prune phase on tool results over 8,000 characters, commit only when it reclaims at least 4,096 tokens, and rearm only after the prompt regrows by the reclaimed amount, so cache breaks stay episodic.
**Milestone:** M2

#### Scenario: Disabled by default
- **GIVEN** default config
- **WHEN** the prompt grows below the compaction threshold
- **THEN** no proactive prune SHALL run

### Requirement: Cache breakpoint plan
When the provider supports explicit cache breakpoints the system SHALL place at most 4 per request, all with one TTL: one at the end of each system-prompt tier that differs from the previous tier boundary (at most 2 used for the system prompt) and the remaining ones on the latest cacheable non-system rows; breakpoints SHALL be applied to a per-request copy, never stored in history, and re-planning SHALL never exceed 4.
**Milestone:** M1

#### Scenario: Tiered prompt with long history
- **GIVEN** a system prompt with `static`, `profile` and `session` tiers and 20 history rows
- **WHEN** the request is built for a breakpoint-capable provider
- **THEN** the request SHALL carry exactly 4 breakpoints, 2 in the system prompt and 2 on the last cacheable rows

### Requirement: Prompt cache TTL
`prompt_cache.ttl` SHALL be one of `"5m"` (default), `"1h"`, `"auto"` or `"off"`; `auto` SHALL resolve once per session to `5m` for machine-paced sessions (subagent, cron, webhook, kanban, api, batch) and `1h` otherwise; `off` SHALL disable cache breakpoints; any other value SHALL fail validation with exit 78; a TTL a provider does not support SHALL be lowered to its longest supported TTL with one DEBUG log.
**Milestone:** M1

#### Scenario: Auto for cron
- **GIVEN** `prompt_cache.ttl = "auto"` and a cron session
- **WHEN** the session starts
- **THEN** the TTL SHALL be `5m`

#### Scenario: Auto for the CLI
- **GIVEN** `prompt_cache.ttl = "auto"` and a CLI session
- **WHEN** the session starts
- **THEN** the TTL SHALL be `1h`

# Study: OpenBot

[CopilotKit/OpenBot](https://github.com/CopilotKit/OpenBot) (MIT) is a self-hostable workspace for AI coworkers. It is a governance layer over AG-UI agents, with a fail-closed policy gateway, approvals, audit with run provenance, and one sandboxed computer (Chromium, workspace and shell) per person or Bot. It is the closest open-source reference for the Dots-class features Prometheus is aiming at: an always-on cloud computer, native desktop and mobile apps, and computer use with takeover.

We reverse-engineered OpenBot at commit `3083c34b` into a baseline OpenSpec: 20 capability specs, 441 requirements and 813 scenarios, each requirement citing `file:line`, all passing `openspec validate --all --strict`.

- **Full specs (PDF, 167 pages):** [openbot-openspec-baseline.pdf](openbot-openspec-baseline.pdf)
- **Tracking issue:** [#33 Epic: Dots-class always-on agent](https://github.com/CyberdyneCorp/prometheus-agent/issues/33)

## Issues opened from this study

| Issue | Gap |
|---|---|
| [#18](https://github.com/CyberdyneCorp/prometheus-agent/issues/18) | Native iOS and Android apps on a shared Rust client core |
| [#19](https://github.com/CyberdyneCorp/prometheus-agent/issues/19) | Mobile push with secure actionable approvals |
| [#20](https://github.com/CyberdyneCorp/prometheus-agent/issues/20) | Watch, take over and return control of the agent's computer |
| [#21](https://github.com/CyberdyneCorp/prometheus-agent/issues/21) | Always-on cloud deployment |
| [#22](https://github.com/CyberdyneCorp/prometheus-agent/issues/22) | Computer supervisor: a persistent cloud computer per bot |
| [#23](https://github.com/CyberdyneCorp/prometheus-agent/issues/23) | Connected personal computer |
| [#24](https://github.com/CyberdyneCorp/prometheus-agent/issues/24) | Browser bot-check resilience |
| [#25](https://github.com/CyberdyneCorp/prometheus-agent/issues/25) | Human-typed sign-in and secret entry |
| [#26](https://github.com/CyberdyneCorp/prometheus-agent/issues/26) | Custom rules with a hand-off outcome |
| [#27](https://github.com/CyberdyneCorp/prometheus-agent/issues/27) | Run provenance in a signed run assertion |
| [#28](https://github.com/CyberdyneCorp/prometheus-agent/issues/28) | Proactive research mode (read-only) |
| [#29](https://github.com/CyberdyneCorp/prometheus-agent/issues/29) | Event-triggered goals and a failure rule |
| [#30](https://github.com/CyberdyneCorp/prometheus-agent/issues/30) | Notification routing rules |
| [#31](https://github.com/CyberdyneCorp/prometheus-agent/issues/31) | Learn from demonstration |
| [#32](https://github.com/CyberdyneCorp/prometheus-agent/issues/32) | Action journal with best-effort undo |
| [#17](https://github.com/CyberdyneCorp/prometheus-agent/issues/17) | AG-UI endpoint (`pmt-agui`) |

The comparison below was written against `main` at `b2f5a502`; the issues were checked against `m3-upgrades` at `e3f8a136`. Paths in the form `openspec/specs/...`, `shared/...`, `app/...`, `agent-*/...` and `desktop/...` refer to the OpenBot repository.

## 1. The two projects in one paragraph each

**OpenBot** is a *workspace and governance layer* over agents. It does not own the agent loop: every
Bot is an AG-UI endpoint (13 framework harnesses in `agent-*/`, catalogued in `shared/harnesses.json`).
OpenBot owns what is around the loop: identity/orgs, a fail-closed policy gateway on every tool call,
approvals, an audit trail with run provenance, one sandboxed computer (Chromium + workspace + shell)
per person or Bot, egress policy, multi-Bot conversations, routines/goals, memory and Learning.

**Prometheus** is a *single-agent runtime* (Hermes parity, 47 crates). It owns the loop: turn engine,
providers, compaction, tool dispatcher, OS sandbox, MCP, skills, cron, delegation, messaging adapters,
TUI/web/desktop/ACP/OpenAI-compatible API. Its open items include computer use (76.x), bot mode and group
chats (75.x), egress credential injection and container proxy coverage (72.8, 73.2), delegation fan-in
(72.11), memory decay (72.x), and no AG-UI.

The two fit together: OpenBot's strength is exactly Prometheus's open backlog, and Prometheus could
be a harness in OpenBot's catalogue.

## 2. Where Prometheus is already ahead (don't copy OpenBot here)

| Area | Prometheus | OpenBot |
|---|---|---|
| Parallel tools | Resource-claim DAG, concurrency 8 (`pmt-agent/src/turn/tools.rs`) | Prefix lanes: `computer_*` serial, rest concurrent (`agent-langgraph-agui/src/tool_runtime.py:495-518`) |
| Sandbox | Landlock/seccomp/Seatbelt/AppContainer (`pmt-sandbox`) | Container per computer, optional gVisor `runsc` |
| Audit integrity | Hash-chained `security-audit.jsonl` + verify (`pmt-core/src/audit.rs`) | Postgres rows, no chain |
| Compaction | Aux-model summary with quality check (`pmt-agent/src/compress/`) | Per-harness replay budgets, inconsistent (see §6) |
| Providers | ~20 hosted + local, pools, cost ledger | Data-driven JSON, 3 providers + OpenAI-compatible |
| MCP | stdio + HTTP, breaker, server mode | Server-side registration, allowlist |

## 3. P0: fill Prometheus's open backlog with OpenBot's designs

### 3.1 Add an AG-UI surface (no task yet), so Prometheus can run inside OpenBot
New crate `pmt-agui` beside `pmt-api`, implementing `openspec/specs/agent-harness-contract/spec.md`:
- **Token gate**: everything except `GET /health` needs `x-openbot-agent-token` equal to
  `MANAGED_AGENT_TOKEN`, compared in constant time and refused when unset; `401 {"error":"unauthorised"}`.
- **Stateless whole-thread runs, session as cache.** Every run carries the full thread. A provider or
  daemon session is only a cache keyed by `thread_id + hash(instructions)` and dropped when it missed a
  turn (`agent-adk/src/main.py:175-223`, `agent-langgraph-agui/src/whole_thread.py:57-70`).
- **Instructions identified by id, not role.** Only `standing-role:*` and `granted-tools:*` system rows
  go in the system slot. Every other system row (memory, skills) becomes labelled user data, inserted
  at call time and never persisted (`shared/harness_contract.py`).
- **Route tools by owner.** Run in-process only when every call in a step is a deployment (server)
  tool. A surface (browser) tool or an undeclared tool ends the run and hands the calls back with no
  error (`tool_runtime.py:399-417`). This maps onto Prometheus's dispatcher as a new "remote" owner.
- **Tool-call stream slots**: key fragments by (model call, index, else id) and emit START once, all
  ARGS, then END; an unidentifiable fragment is a terminal error (`parallel_tools.py`).
- **Guaranteed terminal event with typed codes**: `MODEL_AUTH_REQUIRED` (provider 401/403 only),
  `BILLING`, `MODEL_REFUSED`, `HARNESS_START_FAILED` and a generic fallback (`tool_runtime.py:291-328`).
  This fits Prometheus's single `ExitReason`.
- **Per-run overrides** `openbotModel` and `openbotSendOnce` come in forwarded props. Strip them and
  hold them in task-local context (a `tokio::task_local!`).
- Then add a row to `shared/harnesses.json` (`credential: any-provider`) and Prometheus is selectable
  in OpenBot Desktop's harness picker.

### 3.2 Computer use (Prometheus tasks 76.1–76.3): adopt the OpenBot computer model
From `openspec/specs/bot-computers/spec.md`:
- **One computer per person (or per isolated Bot)**: Chromium, `/workspace`, shell and file tools in one
  container. A supervisor with only `ensure/stop/reset/list` holds the Docker socket, and the agent never
  does. Idle computers are suspended.
- **Per-computer token**: `HMAC-SHA256(root, "openbot computer token v1\n<kind>:<id>")`
  (`shared/computer-token.ts`). The root never enters a sandbox, and a shell that reads its own token
  opens only its own box. The versioned label allows rotation.
- **Element refs from a snapshot**: the agent acts on refs, and the gateway resolves the target from
  the server-held snapshot, so policy sees `element.role/name/type` and `page.host`, not raw selectors.
- **Handover state machine**: `waiting → taken → completed | cancelled | expired | interrupted`, with
  `heldBy`. While a person holds control, agent actions are refused, not queued. The page URL is kept
  so a restarted computer reopens it. Bot checks and CAPTCHAs are never solved; they always hand over.
- **Secret entry by element ref**: the agent names a field and a person types into the page. The agent
  hears only `supplied` (never the value) and the audit row records the length, not the secret.
- **Screen and terminal** as WebSockets gated by the same "may watch this Bot" check. The terminal is
  one persistent bash so `cd` and the environment carry over.

### 3.3 Group chats and bot mode (75.x) and delegation fan-in (72.11)
From `coworkers-and-conversations` and `bot-orchestration`:
- **One thread per agent plus a shared attributed transcript** (`group_bot_threads`, `group_messages`).
  An agent never sees another agent's private context, only the transcript.
- **`@Name` handoff** under the same grant, caps and audit as any tool call. Attribution goes in the
  message id (`bot-answer:<bot>:<run>`, parsed at the last colon). Depth and target come from the
  signed run assertion, not the model, and caps refuse rather than truncate.
- **Typed handoff briefs** (task, bounds, success criteria). A missing target and a hidden target get
  the same refusal, so the roster can't be enumerated.
- **Derived child-thread ids**: a UUIDv8 of `sha256(tool, parent, call)` that keeps the parent's
  fingerprint (`shared/subagent.ts:86-116`). A UI can show a child's steps before its result exists,
  which is useful for `pmt-delegation` spawn trees.

### 3.4 Egress completion (72.8, 73.2)
- **Fail closed until a policy arrives.** The per-box filter proxy denies everything until the
  controller pushes the policy, and the policy is re-pushed on every wake. Metadata addresses are refused
  even under `allow_all`. The modes are `allow_all | defaults_plus_allowlist | allowlist_only | deny_all`.
- **Credential injection without handing over the token**: the Desktop loopback relay pattern
  (`desktop/src-tauri/src/connected/`) binds `127.0.0.1` only, checks `Host` (else 421), uses a per-run
  secret cookie, injects the bearer only on API paths, strips forwarding headers and never follows
  redirects. This is the shape 72.8 needs.

## 4. P1: governance upgrades for `pmt-tools/src/approval` and `pmt-core`

1. **A declarative, fail-closed policy layer** after Prometheus's hardline blocks
   (`action-policy-gateway`): CEL rules over a typed context (`tool.name`, `intent`, `page.host`,
   `element.*`, `command`, `file.path/extension`, `mcp.server/tool/effect`, `initiator.kind/id`,
   `actor.id`). Deny rules run before allow rules. A deny rule that errors denies; an allow rule that
   errors does not permit; an empty policy permits nothing; a malformed policy stops startup. `dry-run`
   records a `trial` verdict. Rust: the `cel-interpreter` crate.
2. **Run provenance in every audit row.** `initiator_kind ∈ {person, deployment, routine,
   responsibility, memory, handoff}` and `initiator_id`, carried in a signed run assertion with `depth`.
   An unknown kind is read as `person`, so an agent can't relabel its own run. Add both fields to the
   hash-chained record. "Nobody watching" (`routine|handoff|responsibility|memory`) becomes a filter.
3. **Record before acting**: decision row, then act, then a second row on failure, both sharing an
   `attempt` id.
4. **Layered what-if / "why did this happen"**: one verdict per layer with `decides`/`unreached` flags,
   computed by the real deciders with no model. The same code explains a past audit row.
5. **One table of outcome sentences** for both the model and the person, with a test that person-facing
   text never contains model instructions. Every "Not run:" ends with the same two ways forward.
6. **Answer-not-kept idempotency** for sensitive results (exec, file read): run at most once per key and
   answer a repeat with 409 instead of caching the output.
7. **A model drafts rules and a blind second call reads them back** against a closed vocabulary
   (two tries, nothing saved by that step). This is a safe way to let users write approval rules in
   English.

## 5. P2: loop, memory, scheduling

- **Replay budget rules** (`agent-claude-sdk/src/replay.py:99-302`), to compare against `compress/`:
  pin the first user message and keep everything from the latest user message on. Cut old tool results
  to 4,000 chars before dropping messages. Never split a call from its result. Insert an omission
  notice. On "prompt is too long", retry once with every result cut. Lesson learned: 2.5 chars/token
  underestimated and caused overruns, so they moved to **1.5**.
- **A client tool inside a live model loop** (`adapter.py:946-1250`): park a future per tool-use id,
  resolve it when the next request brings the result, and replace the loop if the user spoke or the tool
  set changed. Never fabricate a result on cancel or timeout. Use an inactivity deadline that each event
  pushes back.
- **Tool budget**: past 50 tools, extra tools wait behind `search_tools`, with a hard cap of 128 per
  request (`shared/model-tool-limit.ts`). Tool names are capped at 64 chars with an 8-hex sha256 suffix
  and mapped back in history.
- **Prompt as tagged sections** in a fixed order, where blank sections disappear and rules vary by
  model. The date at day granularity goes last so the cached prefix stays stable
  (`shared/bot-prompt.ts:736-873`).
- **Scheduling** (`routines-and-goals`), compared with `pmt-cron`:
  - Store the next run as a timestamp and skip missed runs instead of replaying them.
  - Key each firing by the minute it was due.
  - Use a lease with `FOR UPDATE SKIP LOCKED` if Prometheus ever runs multi-replica.
  - The failure rule: one message on the first failure after a success, and switch the job off after 10
    consecutive failures, counted since it was last enabled.
  - Always run as the creator with the creator's grants.
- **Memory** (72.x decay/consolidation): each memory records which agent formed it and a review state.
  Recall only reviewed, enabled memories. Keep a lasting "forgotten" marker so a re-import can't
  resurrect a fact.
- **Skills layered agent → person → shared**: a slow or missing layer is skipped and never fails the
  run, and learned guidance never grants tools.

## 6. P3: operations, from OpenBot Desktop's Rust (`desktop/src-tauri/src`)

- **Closed-enum telemetry**: serde `deny_unknown_fields`, events persisted before sending, a 256-event
  queue that drops the oldest, and a bounded flush on quit (3 s per request, 6 s total)
  (`telemetry.rs`).
- **Log scrubbing as a mandatory sink filter**: key-, token- and email-shaped text is redacted and the
  home folder is written as `~`. A compile-time `const _: () = assert!(..)` checks the rotation size
  (`logging.rs:42`).
- **Child-process boundary**:
  - One `command()` constructor for every spawn (`quiet.rs`).
  - Binaries pinned by SHA-256, with no PATH fallback.
  - Commands as an argv enum, so illegal commands can't be represented.
  - Output capped at 1 MiB.
  - Only a static failure category crosses IPC (`local_eval.rs`).
- **Transactional upgrades**: `.previous/`, a verified DB dump, start, health check, then GC. Any
  failure restores everything, and a leftover `.previous/` blocks the next run until repaired. This is
  relevant to `pmt-update` and `pmt-backup`.
- **Stall detection kept separate from failure**: a step that takes 3× its typical time and at least
  2 min over, plus a 30 s UI heartbeat.
- **Dependency-injected clocks and probes**, with a wall-clock check every 5 min so schedules survive
  laptop sleep (`app_update.rs`).
- **CI**:
  - A test-count floor summed across shards, so a shrinking suite fails (`scripts/test-floor.ts`).
  - Breaking-change diffs for REST and WebSocket schemas with an accepted-changes ledger, which could be
    applied to `schema/rpc.json`.
  - Publish then promote: a release becomes Latest only after every signed artifact is attached.

## 7. Suggested order of work

1. `pmt-agui` (§3.1). It is small and makes Prometheus usable as an OpenBot harness. That gives you
   OpenBot's governance, computers and multi-Bot UX for free, and a live test bed for 2–4.
2. Computer use on the OpenBot model (§3.2), which closes 76.x.
3. Policy layer plus provenance in audit (§4.1–4.3), extending the existing approval chain and hash
   chain.
4. Group chats and handoffs (§3.3), egress completion (§3.4).
5. Replay, scheduling and memory refinements (§5) as the related tasks come up.

## 8. OpenBot open questions (found while writing the specs)

About 75 doc/code mismatches and suspected bugs, each with file:line, are recorded inside the specs as
"(as-is)" or "Flag" notes. The highest-signal ones:
- `readTeamBotConsent` (`app/src/lib/team-bots.ts:167`) and `readConnectPrompt`
  (`shared/connect-prompt.ts:131`) cut JSON at the first `}`, so a value containing `}` drops the card.
- The extension hardcodes `ORGANIZATION_ID = "default"` (`extension/src/shared/openbot-client.ts:77`)
  after multi-org phase 2.
- The Tauri CSP is `null` (`desktop/src-tauri/tauri.conf.json:27`).
- The managed LangGraph Bot has no replay budget, unlike the Claude SDK, Agno and Antigravity harnesses.
- The Mastra token check is not constant-time on length (`agent-mastra/src/mastra/index.ts:150`).
- AG2 and Microsoft silently accept `BOT_PROVIDER=google` with an OpenAI client.
- The boundary helpers only edit `deny` rules (`app/src/lib/boundaries/rules.ts:122,132`).
- Dev `COMPUTER_TOKEN`/`SUPERVISOR_TOKEN` defaults have no production refusal (`docker-compose.yml:165,344`).
- The routine minimum interval is hourly in the API but 15 min in the docs (`schedules.yaml:59` vs
  `docs/routines.md:109`).

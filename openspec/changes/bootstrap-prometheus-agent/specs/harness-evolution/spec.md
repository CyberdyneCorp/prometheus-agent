# Harness Evolution

## Purpose
Harness evolution lets Prometheus improve its own **harness** (the editable tree around the fixed model: system prompt overlay, guardrails, memory policy, tool guidance, subagent definitions, skills, scripts and hooks) for a concrete task through a budgeted Propose–Probe–Compose search adapted from Harness². Candidates are probed by re-running the task in isolated sandboxes, composed, and a blind, anonymised, answer-free selector picks one final, which becomes a *proposal* the user approves (or a policy adopts) and which takes effect from the next session only. The whole capability is new relative to Hermes; requirements marked "(upgrade)" go beyond the Harness² research pipeline it is adapted from.

## ADDED Requirements

### Requirement: Opt-in per profile
The system SHALL keep harness evolution disabled unless `evolve.enabled = true` (default `false`) in the profile's `config.toml`, and every `prometheus evolve` subcommand, the `/evolve` slash command and the `evolve.*` RPC methods SHALL fail on a disabled profile with exit code 78 and the message `harness evolution is disabled for profile '<name>'; set evolve.enabled = true`. The `prometheus harness` subcommands (inspection, manual edits, rollback) SHALL work regardless of `evolve.enabled`.
**Milestone:** M3

#### Scenario: Disabled profile
- **GIVEN** a profile whose `config.toml` has no `[evolve]` section
- **WHEN** the user runs `prometheus evolve run --session 01J...`
- **THEN** the system SHALL exit 78 with the disabled message and SHALL NOT create anything under `<profile>/evolve/`

#### Scenario: Harness inspection without evolution
- **GIVEN** `evolve.enabled = false`
- **WHEN** the user runs `prometheus harness log`
- **THEN** the system SHALL list harness versions and exit 0

### Requirement: Harness tree and on-disk layout
The system SHALL represent a harness as a mapping from normalized relative POSIX paths to UTF-8 text, SHALL store every version under `<profile>/harness/versions/<version_id>/tree/` with a `version.toml` (`id`, `parent`, `fingerprint`, `source` ∈ {`builtin`, `manual`, `evolve`, `rollback`, `import`}, `job_id`, `created_at`, `census`), and SHALL record the active version id in `<profile>/harness/ACTIVE`. The fingerprint SHALL be the first 16 hex characters of SHA-256 over every file in sorted path order as `path \0 bytes \0`, and `version_id` SHALL be `h<sequence>-<fingerprint>`. Path normalization SHALL reject absolute paths, `..` segments, symlinks and NUL bytes, and SHALL strip leading `./`.
**Milestone:** M3

#### Scenario: Fresh profile
- **WHEN** a profile is created
- **THEN** `<profile>/harness/ACTIVE` SHALL name `h1-<fp>` whose `version.toml` has `source = "builtin"` and whose always-on files exist and are empty

#### Scenario: Traversal rejected
- **WHEN** a tree containing the key `../config.toml` is validated
- **THEN** validation SHALL fail naming the path and no version SHALL be written

### Requirement: Editable surface and component numbering
The system SHALL treat a path as editable only if it is one of: `SYSTEM.md` (c1 system prompt overlay), `GUARDRAILS.md` (c2 guardrails), `MEMORY_POLICY.md` (c3 memory policy), `TOOL_GUIDANCE.md` (c4 tool guidance), `agents/<name>/AGENT.md` (c5 subagent definitions), `skills/<name>/SKILL.md` or `skills/<name>/scripts/<file>` (c6 skills), `scripts/<file>` (c7 scripts), or `hooks.toml` (c8 hooks), where `<name>` matches `[a-z0-9][a-z0-9_-]{0,63}` and `<file>` matches `[a-z0-9_][a-z0-9_.-]{0,63}\.(py|sh)`. Each component number SHALL be used for exactly one component. Any other path in a proposed tree SHALL be reported as off-surface and SHALL cause the proposal to be rejected; user memory content, `config.toml`, secrets and installed skills SHALL never be on the surface.
**Milestone:** M3

#### Scenario: Off-surface write
- **WHEN** a proposal writes `memories/MEMORY.md` and `scripts/Clean.py`
- **THEN** both paths SHALL be listed as off-surface (at most 6 listed, then `(+N more)`) and the proposal SHALL be rejected

### Requirement: Harness size caps
The system SHALL enforce, at validation time, these caps with the defaults shown: combined always-on text (`SYSTEM.md`, `GUARDRAILS.md`, `MEMORY_POLICY.md`, `TOOL_GUIDANCE.md`) ≤ `harness.always_on_cap_chars = 24000` characters; any single file ≤ 65,536 bytes; whole tree ≤ 1,048,576 bytes; ≤ 200 files; ≤ 32 skills; ≤ 8 subagent definitions. `harness.always_on_cap_chars` SHALL be configurable only within 4,000–64,000.
**Milestone:** M3

#### Scenario: Always-on cap exceeded
- **GIVEN** the four always-on files total 24,001 characters
- **WHEN** the tree is validated
- **THEN** validation SHALL fail with `always-on text is 24001 chars; cap is 24000`

### Requirement: Harness validation
The system SHALL validate a tree by collecting every error (not stopping at the first) and SHALL reject the tree if any of these fail: path and surface rules; size caps; every skill has YAML frontmatter with `name` equal to its directory and a non-empty `description` within the first 600 characters; no harness skill name equals an installed skill name in the profile; every skill is named in always-on text, after auto-trigger repair has appended ``- When <description>, load the `<name>` skill.`` to `GUARDRAILS.md` for each unnamed skill (skipping a line that would exceed the always-on cap); every subagent `AGENT.md` has `name` and `description` frontmatter and names only toolsets known to the toolset registry; every `.py` script parses and every `.sh` script passes a shell syntax check, without executing either; every `scripts/<file>` or `skills/<name>/scripts/<file>` reference in harness text exists in the tree; `hooks.toml` names only hook events in the hook registry and its commands reference only harness scripts; always-on text contains no `{{` or `{%`; and no file matches the secret scanner. On success it SHALL return a census `{always_on_chars, files, skills, agents, scripts, hooks}`. `prometheus harness validate [DIR]` SHALL print every error and exit 65 on failure.
**Milestone:** M3

#### Scenario: Missing referenced script
- **GIVEN** `SYSTEM.md` says "run scripts/clean.py" and no `scripts/clean.py` exists
- **WHEN** `prometheus harness validate` runs on the tree
- **THEN** it SHALL exit 65 naming `scripts/clean.py`

#### Scenario: Skill without trigger line is repaired
- **GIVEN** a tree adds `skills/recon/SKILL.md` with `description: reconciling ledgers` and no mention in always-on files
- **WHEN** the tree is validated
- **THEN** `GUARDRAILS.md` SHALL gain ``- When reconciling ledgers, load the `recon` skill.`` and validation SHALL pass

#### Scenario: Unknown hook event
- **WHEN** `hooks.toml` declares an `on_magic` event
- **THEN** validation SHALL fail with `unknown hook event 'on_magic'`

### Requirement: Harness loaded once per session
The system SHALL resolve `<profile>/harness/ACTIVE` exactly once at session start, record the version id on the session row (`sessions.harness_version`), and inject the always-on files as one harness layer of the system prompt in the fixed order c1, c2, c3, c4 immediately after the built-in base prompt and before identity and context files; harness skills and subagents SHALL be registered for that session only. A harness version adopted, rolled back or edited while a session is open SHALL NOT change that session's system prompt, tools or skills, except through an explicit `--now` which SHALL rebuild the prompt at the next turn boundary and log `prompt cache invalidated by harness change` at WARN.
**Milestone:** M3

#### Scenario: Adoption mid-session
- **GIVEN** an open session started on `h3-aaaa`
- **WHEN** the user adopts a proposal creating `h4-bbbb` without `--now`
- **THEN** the open session's next request SHALL carry a byte-identical system prompt and a new session SHALL record `harness_version = "h4-bbbb"`

### Requirement: Harness execution confinement
The system SHALL run harness scripts and hook commands only inside the session's execution sandbox under the session's existing capability grants; a harness SHALL NOT be able to declare, request or widen any capability grant, network egress rule, secret access or approval mode, and validation SHALL reject any `hooks.toml` key other than `event`, `command`, `timeout_secs` (1–120, default 30) and `matcher`.
**Milestone:** M3

#### Scenario: Hook asking for network
- **WHEN** `hooks.toml` contains `network = ["api.example.com"]`
- **THEN** validation SHALL fail with `unknown key 'network' in hooks.toml`

### Requirement: Harness history and diff
The system SHALL provide `prometheus harness log` (versions newest first with id, source, job id, created time, census and an `*` on the active one), `prometheus harness show [VERSION] [PATH]`, and `prometheus harness diff <A> [<B>]` (B defaults to the active version) printing a unified diff plus the summary `{added, removed, modified, always_on_char_delta}`. Versions SHALL be immutable once written and SHALL never be deleted while referenced by `ACTIVE`, a pending proposal or an experience-library entry.
**Milestone:** M3

#### Scenario: Diff summary
- **GIVEN** `h5` adds `skills/recon/SKILL.md` and grows `GUARDRAILS.md` by 52 characters relative to `h4`
- **WHEN** the user runs `prometheus harness diff h4 h5`
- **THEN** the summary SHALL be `added: [skills/recon/SKILL.md], removed: [], modified: [GUARDRAILS.md], always_on_char_delta: 52`

### Requirement: Rollback and manual edits
The system SHALL provide `prometheus harness rollback <VERSION> [--now]`, which writes a new version with `source = "rollback"` and the target's tree and makes it active, and `prometheus harness apply <DIR> [--now]` / `prometheus harness edit`, which validate a user-edited tree and write a new version with `source = "manual"`. Both SHALL take effect from the next session unless `--now` is given, and SHALL emit the `harness.activated` event with `{from, to, source}`.
**Milestone:** M3

#### Scenario: Rollback creates a new version
- **GIVEN** versions `h1`..`h6` with `h6` active
- **WHEN** the user runs `prometheus harness rollback h4`
- **THEN** a version `h7-<fp>` with `source = "rollback"` and the same fingerprint as `h4` SHALL become active and `h5`, `h6` SHALL remain in the log

### Requirement: On-demand and scheduled triggers
The system SHALL start an evolution job from: `prometheus evolve run (--session <ID> | --task <FILE>) [--k N] [--mode parallel|sequential|both] [--domain D] [--budget-usd X] [--detach]`; the `/evolve [--k N]` slash command, which queues a job for the current session and returns immediately without running inside a turn; a scheduled job whose payload is `kind = "evolve"`; and a kanban task of kind `evolve`. Every trigger SHALL produce the same job record and pass through the same budget and adoption rules.
**Milestone:** M3

#### Scenario: Slash command does not block
- **WHEN** the user types `/evolve` in a chat session
- **THEN** the system SHALL reply with the job id within the same turn boundary, SHALL NOT add the job's work to the current turn, and SHALL NOT alter the current session's prompt

#### Scenario: Cron-triggered evolution
- **GIVEN** a scheduled job with `kind = "evolve"` and `task = "tasks/weekly-report.toml"`
- **WHEN** the job fires
- **THEN** an evolution job SHALL be queued with `trigger = "cron"` and the scheduled job's id

### Requirement: Automatic triggers after failed or long tasks
The system SHALL queue an evolution job automatically only when `evolve.auto.enabled = true` (default `false`) and a finished session either ended in error or interruption after at least one tool call (`evolve.auto.on_failure = true`), or used at least `evolve.auto.min_iterations = 40` iterations (`evolve.auto.on_long_task = true`); automatic jobs SHALL be limited to `evolve.auto.max_jobs_per_day = 3` per profile, SHALL never be queued from sessions whose `source` is `evolve`, and SHALL notify the user that a job was queued.
**Milestone:** M3

#### Scenario: Daily cap
- **GIVEN** three automatic jobs already queued today
- **WHEN** a fourth qualifying session ends in error
- **THEN** no job SHALL be queued and the decision SHALL be logged at INFO with reason `daily auto-evolve cap reached`

### Requirement: Evolution task capture
The system SHALL freeze each job's task into `<profile>/evolve/jobs/<job_id>/task/` containing `task.md` (the task statement: by default the session's first user message, editable via `--edit` before the job starts), `source/` (a snapshot of the workspace files the session read or modified, or, when the workspace is a git repository, `repo.toml` with the HEAD commit plus `uncommitted.patch`), and `task.toml` (`domain`, `toolsets`, `model`, `backend`, `source_session`, `fingerprint`). The snapshot SHALL be capped at `evolve.task.max_source_bytes = 52428800`, SHALL pass the redaction pass of `security-boundaries`, and SHALL exclude files matching leak globs; a task exceeding the cap SHALL fail with exit 65.
**Milestone:** M3

#### Scenario: Git workspace
- **GIVEN** a session that edited files in a git repository with uncommitted changes
- **WHEN** a job is created from it
- **THEN** `task/source/repo.toml` SHALL record the HEAD commit and `uncommitted.patch` SHALL reproduce the uncommitted state when applied to that commit

### Requirement: Modes, budget and labels
The system SHALL accept recursion budget `k` in 1–8 (default `evolve.k = 2`) and mode `parallel` or `sequential` (default `evolve.mode = "parallel"`), or `both`, which runs each mode on the same base rollouts and yields one proposal per mode of which at most one may be adopted. Each mode SHALL spend exactly R = 4k rollouts and P = 3k improver sessions, with base rollouts labelled `base1..basek` and shared across modes, proposal stages `inv<i>` (general edits) and `dom<i>` (domain edits), parallel compositions `agg<i>` and sequential checkpoints `c<t>`. If no base rollout is ok the job SHALL end with status `base-failed` and error `no base rollout produced anything`, and no mode SHALL run.
**Milestone:** M3

#### Scenario: Parallel k=2 labels
- **WHEN** parallel mode runs with k=2
- **THEN** proposal stages SHALL be `inv1, dom1, inv2, dom2` in that order and compositions `agg1, agg2`

#### Scenario: Invalid k
- **WHEN** the user runs `prometheus evolve run --session 01J... --k 9`
- **THEN** the system SHALL exit 2 with `--k must be between 1 and 8`

### Requirement: Parallel mode
In parallel mode the system SHALL run the 2k proposal stages with identical evidence (base rollouts, active harness, experience library, no pool, no decision notes), roll every candidate tree once (including fallen-back copies), run k independent compositions each seeing the base rollouts, the active harness, a pool of all 2k candidates with their rollouts and diffs and every candidate's decision notes, and each editing a fresh copy of the active harness, roll each composition once, and record `similarity = {n, n_distinct, mean_line_jaccard}` over the k final trees.
**Milestone:** M3

#### Scenario: Parallel ledger
- **WHEN** parallel mode completes with k=3
- **THEN** the ledger SHALL record 12 rollouts (3 base, 6 candidates, 3 finals) and 9 sessions

### Requirement: Sequential mode
In sequential mode the system SHALL, for t = 1..k, run `inv<t>` and `dom<t>` against the base rollouts, the current checkpoint (the active harness for t=1, else `c<t-1>`), a pool of all earlier stages and their decision notes; roll both; compose `c<t>` from the full history plus this round's candidates by editing a copy of the current checkpoint; roll `c<t>`; and carry it forward. All checkpoints `c1..ck` SHALL be eligible finals, and sequential prompts SHALL NOT describe candidates as independent draws.
**Milestone:** M3

#### Scenario: Second round sees history
- **WHEN** sequential mode runs with k=2
- **THEN** the evidence for `inv2` SHALL contain `pool/c1/rollout.md` and `decisions/c1.md`

### Requirement: Isolated rollouts
The system SHALL execute every rollout as a headless, non-interactive session (`source = "evolve"`) with the candidate harness, in a fresh sandbox workspace (a detached git worktree at the recorded commit with the patch applied, or a copy of `task/source/`) on backend `evolve.rollout.backend` (default `local-sandbox`; the unsandboxed host backend SHALL be rejected), with network egress limited to the configured model providers unless `evolve.rollout.network = "session"` reuses the source session's egress rules. Rollouts SHALL auto-deny every approval request, SHALL NOT write memory, SHALL be hidden from session lists, search and memory nudges, and SHALL be bounded by `evolve.rollout.max_iterations = 60` and `evolve.rollout.timeout_secs = 900`. A rollout that errors SHALL be recorded as `ok = false` with the error, never aborting the job; a rollout is `ok` when it ends with a final assistant message.
**Milestone:** M3

#### Scenario: Approval inside a rollout
- **WHEN** a rollout's agent calls a command that requires approval
- **THEN** the command SHALL be denied, the denial SHALL appear in the trajectory, and no prompt SHALL reach any user surface

#### Scenario: Workspace untouched
- **WHEN** a rollout deletes files in its workspace
- **THEN** the user's real workspace SHALL be byte-identical before and after the job

### Requirement: Side-effect dry-run in rollouts (upgrade)
The system SHALL replace, during rollouts, every tool whose registry entry declares `effect = "external"` (messaging and gateway delivery, email, scheduled-job and kanban mutation, memory writes, delegation outside the rollout sandbox, browser actions on hosts outside the egress allowlist) with a recorder that stores the call arguments in the trajectory and returns `recorded, not executed during harness evolution`. Rollout deliverables SHALL consist of the final assistant message, the workspace diff, and the list of recorded external calls.
**Milestone:** M3

#### Scenario: Message not sent
- **WHEN** a rollout calls `send_message` to a Telegram chat
- **THEN** no message SHALL be delivered and the deliverable SHALL list the recorded call with its arguments

### Requirement: Improver sessions
The system SHALL run every propose, compose, select and reflect step as an improver session using `evolve.improver.model` (default: the `evolution` auxiliary task route of `model-providers`, falling back to the profile's main model) in a fresh private copy of the evidence directory, with a toolset limited to file read, write, list and search confined to that directory, and no terminal, network, browser, memory or delegation tools. A session SHALL be retried once on transport failure or empty reply after 10 s; on exhaustion it SHALL fail with its accumulated cost recorded. Session timeouts SHALL default to `evolve.improver.timeout_secs = 1800` and `evolve.improver.max_iterations = 120`; temporary directories SHALL be removed when the session ends.
**Milestone:** M3

#### Scenario: Improver cannot run candidate scripts
- **WHEN** an improver attempts to execute `context/pool/inv1/harness/scripts/fix.py`
- **THEN** no execution tool SHALL be available and the attempt SHALL fail as an unknown tool

### Requirement: Stage acceptance, retry and fallback
The system SHALL accept a stage's edited `context/harness/` only after auto-trigger repair, harness validation and the leak tripwire pass; a rejected proposal (no reply, empty harness, off-surface paths or validation errors) SHALL be retried once with the session mission suffixed by `# YOUR PREVIOUS ATTEMPT WAS REJECTED` and the error list, starting from the model's own rejected edit. After two rejections, an evidence-build failure, or transport exhaustion (which SHALL NOT trigger a stage retry), the stage SHALL inherit its fallback tree (the active harness in parallel mode, the current checkpoint in sequential mode) with `fell_back = true` and the error truncated to 400 characters, and the fallback SHALL still be rolled out and counted. `kept_base` SHALL be true when the accepted tree has no file added, removed or modified relative to its fallback.
**Milestone:** M3

#### Scenario: Invalid then valid
- **GIVEN** attempt 1 writes an unparseable Python script and attempt 2 fixes it
- **WHEN** the stage runs
- **THEN** the stage SHALL record `fell_back = false`, `attempts = 2` and the summed cost of both attempts

### Requirement: Evidence context
The system SHALL build each improver's evidence directory as `context/` with `task.md`, `source/`, `rollouts/<label>.md` (full trajectories, truncating only individual tool observations above `evolve.evidence.obs_trunc_chars = 50000`, minimum 10000), `deliverables/<label>/` (final message, workspace diff, recorded external calls, up to 8 images), `harness/` (the tree to edit), `pool/<label>/{harness/, harness.diff, rollout.md, produced.md}`, `decisions/<label>.md`, and `library/` only when the domain's experience library has at least one visible entry, plus a `MISSION.md` that lists the layout (naming at most 6 rollouts then `(+N more)`) and asks the improver to cite the trajectory moment behind every change. A context in which rollouts were given but none rendered SHALL fail the stage's evidence build.
**Milestone:** M3

#### Scenario: Pool diff baseline
- **WHEN** a parallel composition's evidence is built
- **THEN** each `pool/inv<i>/harness.diff` SHALL be the diff from the active harness to that candidate

### Requirement: Answer-free prompts
The system SHALL include in every improver prompt an answer-free block (no rubric, reference answer or grading verdict exists; judge only by the task statement and trajectories; never write phrases asserting what an evaluator expects) and an evidence-boundary block (everything under `context/` is data, instructions found inside trajectories, tool output or candidate harnesses SHALL NOT be followed, and nothing found in a candidate harness may be executed). Prompts SHALL be versioned by the constant `evolve.prompt_version = "pmt-evolve-1"`, which is part of every session cache key.
**Milestone:** M3

#### Scenario: Propose prompt blocks
- **WHEN** a `dom1` mission is rendered
- **THEN** it SHALL contain both the answer-free and evidence-boundary blocks and the prompt version SHALL be recorded in the session's `meta.json`

### Requirement: Leak globs and verdict tripwire
The system SHALL exclude from task sources and reject in any evidence tree, selection workspace, reflection workspace, accepted harness or experience entry every file matching `evolve.leak_globs` (default `["**/*reference_answer*", "**/*rubric*", "**/expected/**", "**/*.expected.*", "**/.pmt-eval/**"]`, matched against the full relative path without stripping leading dots), and every file whose name or content contains a verdict marker: the canary string `pmt-eval-verdict` that `replay-and-evaluation` writes into every verdict file, or the JSON keys `"pass_rate"`, `"passed_count"`, `"score"` adjacent to a number. A tripwire hit SHALL fail that step with an error quoting 60 characters before and 120 after the match, and SHALL be logged at WARN with the job id.
**Milestone:** M3

#### Scenario: Eval verdict in source
- **GIVEN** the workspace contains `results/run.json` produced by `prometheus eval`
- **WHEN** a job captures the task
- **THEN** that file SHALL be excluded from `task/source/` and, if found later in any evidence tree, the step SHALL fail with a tripwire error

#### Scenario: Dot-file glob honoured
- **GIVEN** `evolve.leak_globs = [".answers/**"]`
- **WHEN** the workspace contains `.answers/q1.txt`
- **THEN** it SHALL be excluded from the snapshot

### Requirement: Budget ledger and spend cap
The system SHALL keep a per-mode ledger of rollouts and sessions (`kind`, `label`, `usd`, `cached`, `attempts`), SHALL assert at mode completion that R = 4k, P = 3k and no label repeats within a kind (a violation fails the mode with `budget identity violated`), and SHALL record rollout, improver, selection and reflection costs separately in `ledger.json`. Before starting, the system SHALL print an estimate; it SHALL stop a job with status `budget-exceeded` as soon as total spend reaches `--budget-usd` or `evolve.budget.max_usd = 5.0`, keeping completed artifacts and producing no proposal. All costs SHALL be charged to the job's root in the observability cost ledger.
**Milestone:** M3

#### Scenario: Cap reached mid-job
- **GIVEN** `--budget-usd 1.00`
- **WHEN** cumulative spend reaches 1.00 during `agg1`
- **THEN** no further model call SHALL start, the job SHALL end `budget-exceeded`, and `prometheus evolve status` SHALL show the spend per kind

### Requirement: Concurrency limits
The system SHALL bound concurrent improver sessions to `evolve.concurrency.sessions = 2` and concurrent rollouts to `evolve.concurrency.rollouts = 2` across all jobs of a gateway process, SHALL run at most one job per (profile, domain) at a time with others queued in FIFO order, and SHALL append ledger entries in the order a serial run would produce regardless of completion order.
**Milestone:** M3

#### Scenario: Out-of-order completion
- **WHEN** candidate rollouts for `dom1` finish before `inv1`
- **THEN** the ledger SHALL list `inv1` before `dom1`

### Requirement: Blind anonymised selection
The system SHALL select one final per mode from a pool of exactly that mode's finals (`agg1..aggk` or `c1..ck`), dropping finals that are not ok or produced no deliverable; candidates SHALL be assigned bijective base-26 ids (`A`, `B`, …) by sorting labels and shuffling with a PRNG seeded from the first 12 hex digits of SHA-256 over `<job_id>|<k>|<mode>`, and the selector SHALL see only `task.md`, `source/` and `candidates/<ID>.md` (with character and line counts) — no harness, label, stage or cost. The reply SHALL contain a fenced `choice` block with one id and a `<why>` (truncated to 2000 characters). At k = 1 no selector session SHALL run and the single surviving final SHALL be chosen. An empty pool, missing choice, unknown id or selector failure SHALL end the mode with `selection failed`, mark the job `partial` and produce no proposal for that mode.
**Milestone:** M3

#### Scenario: Stable ids
- **WHEN** the same job, k and mode are selected twice
- **THEN** every label SHALL receive the same anonymous id both times

#### Scenario: Unknown id
- **WHEN** the selector chooses `Z` but only `A` and `B` exist
- **THEN** the mode SHALL record `selection failed` and no proposal SHALL be created for it

### Requirement: User-driven selection (upgrade)
The system SHALL support `evolve.selection = "user"` (default `"model"`), in which no selector session runs and the job pauses in status `awaiting-selection` while `prometheus evolve compare <job>` (and the dashboard) shows the anonymised candidates side by side; `prometheus evolve choose <job> <ID>` SHALL record the choice with `selected_by = "user"` and continue.
**Milestone:** M3

#### Scenario: User picks a candidate
- **GIVEN** a job paused in `awaiting-selection` with candidates `A` and `B`
- **WHEN** the user runs `prometheus evolve choose <job> B`
- **THEN** the job SHALL proceed to reflection with the final mapped to `B`

### Requirement: Proposals and adoption policy
The system SHALL turn a successful selection into a proposal (`<profile>/evolve/proposals/<proposal_id>.toml` with job id, mode, chosen label, base version, diff summary, census, selector reason, distilled lesson and status ∈ {`pending`, `adopted`, `rejected`, `superseded`, `no-change`}); a chosen tree identical to the base SHALL yield `no-change`. Adoption SHALL follow `evolve.adoption.policy` ∈ {`ask` (default), `auto`, `never`}: `ask` notifies the user and waits for `prometheus evolve adopt <proposal> [--now]` or `prometheus evolve reject <proposal>`; `auto` adopts immediately except that any proposal adding or modifying `scripts/`, skill scripts, `hooks.toml` or `agents/` SHALL always wait for explicit approval; `never` keeps proposals for review only. Adoption SHALL write a new harness version with `source = "evolve"` that applies from the next session, and SHALL emit `evolve.proposal.adopted`.
**Milestone:** M3

#### Scenario: Auto policy with a new script
- **GIVEN** `evolve.adoption.policy = "auto"`
- **WHEN** the chosen final adds `scripts/normalize.py`
- **THEN** the proposal SHALL stay `pending` and the user SHALL be notified that approval is required

#### Scenario: Review before adoption
- **WHEN** the user runs `prometheus evolve review <proposal>`
- **THEN** the system SHALL print the diff against the base version, the census, the selector reason and the lesson

### Requirement: Stale proposal protection
The system SHALL refuse to adopt a proposal whose base version is not the currently active version, exiting 65 with `active harness changed since this proposal was produced (base <A>, active <B>); re-run the job or adopt with --rebase`; `--rebase` SHALL apply the proposal's diff to the active tree only if it applies without conflicts and the result validates. Adopting one proposal SHALL mark other pending proposals with the same base as `superseded`.
**Milestone:** M3

#### Scenario: Manual edit after job
- **GIVEN** a pending proposal based on `h4` and a manual edit that made `h5` active
- **WHEN** the user runs `prometheus evolve adopt <proposal>`
- **THEN** the command SHALL exit 65 with the stale message and `ACTIVE` SHALL remain `h5`

### Requirement: Job lifecycle, events and RPC
The system SHALL persist each job in `<profile>/evolve/jobs/<job_id>/` with `job.toml` and an atomically replaced `summary.json`, with status ∈ {`queued`, `running`, `awaiting-selection`, `cancelled`, `base-failed`, `budget-exceeded`, `partial`, `failed`, `ok`}; expose `prometheus evolve status|list|cancel` and the RPC methods `evolve.start`, `evolve.status`, `evolve.list`, `evolve.cancel`, `evolve.choose`, `evolve.proposals.list`, `evolve.proposals.adopt`, `evolve.proposals.reject`, `harness.log`, `harness.diff`, `harness.rollback`; emit trajectory events `evolve.job.started`, `evolve.stage.finished`, `evolve.rollout.finished`, `evolve.selection.finished`, `evolve.proposal.ready`, `evolve.job.finished`; and prune job directories older than `evolve.retention_secs = 2592000` except the summary of jobs whose proposal was adopted. `cancel` SHALL stop the job at the next model-call boundary with status `cancelled`. A foreground `evolve run` SHALL exit 0 when the job ends `ok`, and 1 otherwise.
**Milestone:** M3

#### Scenario: Cancel
- **WHEN** the user runs `prometheus evolve cancel <job>` during `inv2`
- **THEN** no new model call SHALL start, in-flight rollouts SHALL be stopped and their sandboxes removed, and the job SHALL end `cancelled` with spend recorded

### Requirement: Resume and content-addressed session cache
The system SHALL cache every improver session under `<profile>/evolve/cache/sessions/<key>/` (`mission.md`, `response.md`, `meta.json`, `workspace/`), where `key` is SHA-256 over the stage label, mission, system prompt, improver model id, prompt version, and every evidence file's path and bytes, and every rollout under `<profile>/evolve/cache/rollouts/<key>/` keyed by task fingerprint, harness fingerprint, model id, rollout label and seed; a cache hit SHALL cost 0 and be ledgered with `cached = true`. A job interrupted by crash or restart SHALL become `queued` again when the gateway starts (if `evolve.resume_on_start = true`, the default) or on `prometheus evolve resume <job>`, and resuming a fully completed job SHALL make zero model calls and zero rollouts. Resume SHALL re-run any stage whose cached artifacts are missing or whose harness fingerprint no longer matches.
**Milestone:** M3

#### Scenario: Resume after crash
- **GIVEN** a k=2 parallel job killed after `agg1` finished
- **WHEN** the gateway restarts
- **THEN** the job SHALL resume and only `agg2`, its rollout and selection SHALL make new model calls

#### Scenario: Edited cached harness
- **GIVEN** a cached `inv1` workspace whose `scripts/fix.py` was modified on disk
- **WHEN** the job resumes
- **THEN** `inv1` and everything depending on it SHALL be re-run

### Requirement: Offline demo pipeline (upgrade)
The system SHALL ship `prometheus evolve demo [--k N] [--mode parallel|sequential|both] [--output DIR]` that runs the full pipeline (base rollouts, propose, probe, compose, select, reflect, library, resume) offline against the deterministic mock provider of `replay-and-evaluation` on two built-in ledger-summing tasks in domain `accounting` whose base harness script sums only positive amounts; with k ∈ {1, 2} the chosen deliverables SHALL be `Total: 85.15` and `Total: 20.25`, the command SHALL exit 0, and an immediate re-run SHALL report both tasks resumed with zero model calls. The demo SHALL run without `evolve.enabled` and without credentials, in a temporary profile.
**Milestone:** M3

#### Scenario: Demo resume
- **GIVEN** a completed `prometheus evolve demo --k 2 --output /tmp/d`
- **WHEN** the same command runs again
- **THEN** it SHALL exit 0 and report `0 model calls, 0 rollouts`

### Requirement: Human-only evaluation of proposals (upgrade)
The system SHALL support `prometheus evolve review <proposal> --eval <suite>`, which runs an eval suite of `replay-and-evaluation` against both the base and the proposed harness and shows the results to the user only; eval results SHALL be written outside `<profile>/evolve/` with the verdict canary, SHALL NOT be stored in proposals, harness versions or experience entries, and SHALL never be shown to any improver, selector or reflector.
**Milestone:** M3

#### Scenario: Verdict does not reach the library
- **GIVEN** a proposal reviewed with `--eval smoke` and then adopted
- **WHEN** the next job in that domain builds evidence
- **THEN** no file in that evidence SHALL contain the verdict canary or the suite's results

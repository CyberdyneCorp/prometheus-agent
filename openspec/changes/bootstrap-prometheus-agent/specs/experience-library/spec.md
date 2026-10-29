# Experience Library

## Purpose
The experience library carries what harness evolution learned from one task to later work in the same **domain**: after a selection, an Experience Reflector distils at most one transferable lesson, and when the user adopts the proposal the adopted harness tree and its `EXPERIENCE.md` are stored in a per-profile, per-domain library. Later evolution jobs in that domain show the library to their proposers, and (opt-in) later sessions in that domain see the lessons as a frozen prompt layer. Retrieval is by construction: a domain directory holds only same-domain entries, whole harness trees are stored rather than summaries, and nothing derived from a grader, rubric or eval verdict may enter it. The capability is new relative to Hermes; "(upgrade)" marks behavior beyond Harness².

## ADDED Requirements

### Requirement: Library layout
The system SHALL store the library at `<profile>/experience/<domain>/` with `INDEX.md` and one directory per entry `entries/<entry_id>/` containing `EXPERIENCE.md`, `entry.toml` and `harness/` (a full copy of the adopted harness tree), where `entry_id` is `<yyyymmdd>-<task_slug>-<6 hex>` and `task_slug` is the job's task title lower-cased with runs of characters outside `[a-z0-9]` replaced by `-` and truncated to 40 characters. The library SHALL exist only when `evolve.enabled = true`, and SHALL be profile-local (no reads from other profiles).
**Milestone:** M3

#### Scenario: Entry created
- **WHEN** a proposal for task "Reconcile Q3 ledger" in domain `accounting` is adopted on 2026-10-29
- **THEN** a directory matching `<profile>/experience/accounting/entries/20261029-reconcile-q3-ledger-<6 hex>/` SHALL contain `EXPERIENCE.md`, `entry.toml` and `harness/`

### Requirement: Domain names
The system SHALL accept domain names matching `[a-z0-9][a-z0-9-]{0,47}`, SHALL normalise user input by lower-casing and replacing runs of other characters (including `/`, `\` and whitespace) with `-` and trimming `-`, SHALL map an empty result to `general`, and SHALL reject a name that still fails the pattern with exit 2.
**Milestone:** M3

#### Scenario: Slash in domain
- **WHEN** the user passes `--domain "Business / Financial Ops"`
- **THEN** the domain SHALL be `business-financial-ops`

### Requirement: Domain resolution for a job
The system SHALL resolve a job's domain in this order: the `--domain` flag; the `domain` field of the triggering scheduled job or kanban task; the source session's `domain` tag (set with `/domain <name>` or `prometheus chat --domain`); otherwise one auxiliary classification call that SHALL choose among the profile's existing domains plus `general` and SHALL NOT invent a new domain. The resolved domain and its source (`flag`, `cron`, `kanban`, `session`, `classifier`) SHALL be recorded in `task.toml`, and classifier cost SHALL be charged to the job outside the R/P identity.
**Milestone:** M3

#### Scenario: Classifier limited to known domains
- **GIVEN** existing domains `accounting` and `devops` and no explicit domain
- **WHEN** the classifier replies `marketing`
- **THEN** the domain SHALL be `general` with source `classifier` and a WARN log `classifier returned unknown domain 'marketing'`

### Requirement: Reflection workspace
The system SHALL, after a mode's selection succeeds (model or user selection, not a failure) and before the proposal is marked ready, build `jobs/<job_id>/reflection/<mode><k>/` containing every rollout of that mode (base, candidates, finals), the chosen harness as `harness/`, the active base harness as `base_harness/`, a pool of every non-base candidate, every stage's decision notes and `adoption.json = {chosen_label, candidate_ids, reason}`, and SHALL run the leak-glob check and verdict tripwire over it before any reflector session starts.
**Milestone:** M3

#### Scenario: Failed selection skips reflection
- **WHEN** a mode's selection failed
- **THEN** no reflection directory SHALL be created and the library SHALL be unchanged

### Requirement: Experience distillation
The system SHALL run one reflector improver session (label `experience.<mode><k>`) that replies with an `<edits>` block giving, per changed component, the lines `component:`, `candidates:`, `change:`, `observed:` and `verdict:` ∈ {`helped`, `hurt`, `no observed effect`, `confounded`}, followed by either an `<experience>` block with single-line `cue:`, `lesson:`, `evidence:` and `confidence:` (`high`, `medium` or `low` plus a reason) or the single line `NO_ENTRY`. The prompt SHALL instruct the reflector to prefer `confounded` when several components changed together or only one rollout supports a difference, that adoption alone never establishes that an edit helped, and to write at most one lesson. A reply missing `cue`, `lesson` or `evidence`, a `NO_ENTRY` reply, or a session failure SHALL yield no lesson, with its cost still charged to the job.
**Milestone:** M3

#### Scenario: All edits confounded
- **WHEN** the reflector marks every edit `confounded` and replies `NO_ENTRY`
- **THEN** the proposal SHALL carry `lesson = none` and adopting it SHALL write no library entry

### Requirement: Entries are written on adoption only
The system SHALL write a library entry only when a proposal that carries a lesson is adopted (by the user or by policy); rejected, superseded, `no-change` and never-adopted proposals SHALL write nothing. An adopted proposal without a lesson SHALL still create its harness version but SHALL NOT create a library entry, and the review output SHALL state `no transferable lesson; nothing will be added to the experience library`.
**Milestone:** M3

#### Scenario: Rejected proposal
- **GIVEN** a pending proposal with a lesson
- **WHEN** the user runs `prometheus evolve reject <proposal>`
- **THEN** `<profile>/experience/<domain>/entries/` SHALL be unchanged

### Requirement: Entry contents
The system SHALL write `EXPERIENCE.md` with the sections `# <task title>`, `- adopted: <chosen label> (job <job_id>, harness <version_id>)`, `- components: <inventory>`, `## Why this was chosen` (selector reason, or `selected by user`), `## What each edit did` (the `<edits>` rows) and `## Transferable lesson` with `cue`, `lesson`, `evidence` and `confidence` bullets, and `entry.toml` with `entry_id`, `domain`, `job_id`, `harness_version`, `mode`, `k`, `confidence`, `created_at`, `pinned = false`, `status = "active"` and `times_shown = 0`. The inventory SHALL read `<N> skills, <M> scripts, <A> agents, <H> hooks; <C> always-on chars` or `always-on edits only`.
**Milestone:** M3

#### Scenario: Inventory wording
- **WHEN** the adopted harness differs from its base only in `GUARDRAILS.md`
- **THEN** the components line SHALL read `always-on edits only`

### Requirement: Answer-free and secret-free entries
The system SHALL run the leak-glob check, the verdict tripwire and the secret scanner over the entry's harness tree, its `EXPERIENCE.md` and the selector reason before writing an entry, and SHALL refuse to write (logging at WARN and marking the proposal `lesson_rejected`) if any of them fires; adoption of the harness version itself SHALL still proceed.
**Milestone:** M3

#### Scenario: Verdict in lesson
- **WHEN** the distilled evidence line contains `"pass_rate": 1`
- **THEN** no entry SHALL be written and the proposal SHALL record `lesson_rejected: verdict tripwire`

### Requirement: Index
The system SHALL rebuild `INDEX.md` after every entry change, writing `# Experience library — <domain>`, fixed guidance stating that entries are examples to copy and re-verify rather than rules, and either `_(empty — no adopted lessons in this domain yet)_` or one section per visible entry (newest first) with its directory, inventory, confidence and cue.
**Milestone:** M3

#### Scenario: Empty domain
- **WHEN** the first job in a new domain starts
- **THEN** `INDEX.md` for that domain SHALL contain the empty marker

### Requirement: Exposure to proposers
The system SHALL copy into a job's evidence `library/` a snapshot taken at job start of `INDEX.md` and the newest `experience.max_entries_in_evidence = 20` visible entries of the job's domain (pinned entries first), and SHALL omit `library/` entirely when the domain has no visible entry. Visible entries are those with `status = "active"`. Entries adopted after the job started SHALL NOT appear in that job's evidence, including on resume.
**Milestone:** M3

#### Scenario: Index without entries
- **GIVEN** a domain directory holding only `INDEX.md`
- **WHEN** evidence is built
- **THEN** the evidence SHALL contain no `library/` directory

### Requirement: Stream order within a domain
The system SHALL run evolution jobs of one domain strictly one at a time in queue order, so that each job's library snapshot equals the entries adopted before it started; jobs of different domains MAY run concurrently within the concurrency limits of `harness-evolution`.
**Milestone:** M3

#### Scenario: Second job sees the first
- **GIVEN** job J1 in `accounting` whose proposal was adopted with a lesson before job J2 in `accounting` started
- **WHEN** J2 builds its `inv1` evidence
- **THEN** `library/entries/` SHALL list exactly J1's entry

### Requirement: Lessons offered to later sessions (upgrade)
The system SHALL, when `experience.session_lessons = true` (default `false`) and a session starts with a known domain (explicit flag, session tag, scheduled job or kanban task), inject once at session start a prompt layer `## Lessons from earlier work in <domain>` listing `cue → lesson` for at most `experience.session_lessons_max = 5` visible entries with confidence `high` or `medium` (pinned first, then newest), capped at 2,000 characters, after the harness layer. The layer SHALL be frozen for the session (prompt-cache invariant), SHALL be omitted when no domain is known, and each injected entry's `times_shown` SHALL be incremented.
**Milestone:** M3

#### Scenario: Low-confidence lesson excluded
- **GIVEN** the `devops` domain has one `low` and one `high` confidence entry
- **WHEN** a session starts with `--domain devops`
- **THEN** the lessons layer SHALL contain only the `high` entry

#### Scenario: Lesson adopted mid-session
- **WHEN** a lesson is adopted while a `devops` session is open
- **THEN** that session's system prompt SHALL be unchanged and the next `devops` session SHALL include it

### Requirement: Rollback demotes entries (upgrade)
The system SHALL set `status = "rolled-back"` on every entry whose `harness_version` is no longer an ancestor of the active version after `prometheus harness rollback`, hiding it from proposers and sessions, unless the entry is pinned; `prometheus experience restore <entry_id>` SHALL set it back to `active`.
**Milestone:** M3

#### Scenario: Rolling back an adopted harness
- **GIVEN** entry E created when `h5` was adopted
- **WHEN** the user runs `prometheus harness rollback h4`
- **THEN** E SHALL have `status = "rolled-back"` and SHALL NOT appear in the next job's evidence

### Requirement: Library CLI
The system SHALL provide `prometheus experience list [--domain D] [--all]` (entries with id, domain, status, confidence, pinned, times shown, cue), `show <entry_id>`, `domains` (domain names with entry counts), `pin|unpin <entry_id>`, `forget <entry_id>` (removes the entry directory, keeps the harness version) and `restore <entry_id>`, with RPC equivalents `experience.list`, `experience.show`, `experience.domains`, `experience.pin`, `experience.forget`, `experience.restore`. An unknown entry id SHALL exit 1 with `no experience entry '<id>'`.
**Milestone:** M3

#### Scenario: Forget an entry
- **WHEN** the user runs `prometheus experience forget 20261029-reconcile-q3-ledger-a1b2c3`
- **THEN** the entry directory SHALL be removed, `INDEX.md` rebuilt and harness version history unchanged

### Requirement: Retention cap
The system SHALL keep at most `experience.max_entries_per_domain = 50` entries per domain, evicting the oldest unpinned entry (by `created_at`) when a new entry would exceed the cap, and SHALL log each eviction at INFO with the entry id.
**Milestone:** M3

#### Scenario: Eviction
- **GIVEN** a domain holding 50 entries of which the oldest is pinned
- **WHEN** a 51st entry is written
- **THEN** the oldest unpinned entry SHALL be removed and the pinned one kept

### Requirement: Export and import between profiles (upgrade)
The system SHALL provide `prometheus experience export <domain> <FILE.tar.zst>` and `prometheus experience import <FILE> [--domain D]`; import SHALL validate every entry's harness tree, rerun the leak check, verdict tripwire and secret scanner, assign new entry ids, set `status = "active"` and `pinned = false`, record `imported_from` in `entry.toml`, and reject the whole archive (exit 65, nothing written) if any entry fails.
**Milestone:** M3

#### Scenario: Archive with a secret
- **WHEN** an imported archive contains an entry whose `SKILL.md` includes an API key
- **THEN** the import SHALL exit 65 naming the entry and the library SHALL be unchanged

### Requirement: Restore on resume and repair
The system SHALL be able to rebuild a domain's library from adopted proposals: `prometheus experience rebuild [--domain D]` and job resume SHALL re-create any missing entry for an adopted proposal that carries a valid lesson from the proposal record and its harness version, without any model call.
**Milestone:** M3

#### Scenario: Deleted library directory
- **GIVEN** `<profile>/experience/accounting/` was deleted after two adoptions with lessons
- **WHEN** the user runs `prometheus experience rebuild --domain accounting`
- **THEN** both entries SHALL be restored and zero model calls SHALL be made

### Requirement: Library events
The system SHALL emit the trajectory events `experience.lesson.distilled` (job, mode, confidence or `none`), `experience.entry.added`, `experience.entry.evicted`, `experience.entry.status_changed` and `experience.session_lessons.injected` (session, entry ids), so that the dashboard and observability exports can show what each session was taught.
**Milestone:** M3

#### Scenario: Injection observable
- **WHEN** a session starts with two lessons injected
- **THEN** one `experience.session_lessons.injected` event SHALL list both entry ids with the session id

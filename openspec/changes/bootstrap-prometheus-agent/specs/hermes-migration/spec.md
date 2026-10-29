# Hermes Migration

## Purpose
Hermes migration imports an existing Hermes Agent home (configuration, `.env`, credential files, sessions with lineage, memory, identity, skills, scheduled jobs, MCP servers and profiles) into a Prometheus home with one command. The key decisions are: the Hermes home is strictly read-only (the migrator works on a staged snapshot and never opens the live Hermes database), every Hermes key and file gets exactly one recorded disposition in a mapping report, secrets and non-secrets are re-sorted into the secrets store and `config.toml` respectively, ambiguous security-relevant values resolve to the more restrictive interpretation, and a migration ledger makes re-runs idempotent and incremental.

## ADDED Requirements

### Requirement: Migration command surface
The system SHALL provide `prometheus migrate from-hermes` with options `--hermes-home PATH` (default `~/.hermes` on POSIX and `%LOCALAPPDATA%\hermes` on Windows), `--profile NAME` (repeatable) or `--all`, `--into-profile NAME` (only with a single `--profile`), `--only <components>` and `--skip <components>` over the component set `config`, `secrets`, `identity`, `memory`, `skills`, `sessions`, `cron`, `mcp`, `plugins`, `gateway`, plus `--dry-run`, `--json`, `--yes`, `--enable-cron` and `--overwrite`, and SHALL NOT read `HERMES_HOME` or any other Hermes environment variable to locate the source.
**Milestone:** M2

#### Scenario: Unknown component rejected
- **WHEN** the user runs `prometheus migrate from-hermes --only sesions`
- **THEN** the command SHALL exit 2 listing the valid component names

#### Scenario: Missing source
- **WHEN** the user runs `prometheus migrate from-hermes --hermes-home /nowhere`
- **THEN** the command SHALL exit 66 with `No Hermes home found at /nowhere`

### Requirement: Source home is never modified
The system SHALL NOT create, modify, chmod, lock, delete or open for writing any file under the Hermes home, SHALL copy `state.db` and its `-wal` and `-shm` sidecars into a staging directory `<profile>/cache/migrate-<run-id>/` of the destination profile and open only the copy with SQLite, and SHALL remove the staging directory when the run ends.
**Milestone:** M2

#### Scenario: Source tree unchanged
- **GIVEN** a Hermes home whose files' contents, sizes, modes and modification times were recorded
- **WHEN** a full migration and a re-run complete
- **THEN** every recorded attribute of every file under the Hermes home SHALL be unchanged

#### Scenario: Hermes running during migration
- **GIVEN** a running Hermes gateway writing to its `state.db`
- **WHEN** the migration runs
- **THEN** the system SHALL import a consistent snapshot, print a WARN that Hermes is running and changes made after the snapshot will be picked up by a re-run, and SHALL NOT block the Hermes process

### Requirement: Profile discovery and mapping
The system SHALL map the Hermes root profile to Prometheus `default` and each live `profiles/<name>` directory (one with a Hermes identity marker such as `config.yaml`, `.env`, `SOUL.md` or `state.db` and no tombstone under `profiles/.deleted/`) to the same Prometheus name, SHALL migrate the Hermes sticky active profile when neither `--profile` nor `--all` is given, SHALL skip tombstoned and marker-less directories with a report entry, and SHALL require `--into-profile` for a Hermes name that is reserved or invalid in Prometheus.
**Milestone:** M2

#### Scenario: All profiles
- **GIVEN** a Hermes home with root config and live profiles `coder` and `ops`, and a tombstoned `old`
- **WHEN** the user runs `prometheus migrate from-hermes --all`
- **THEN** Prometheus profiles `default`, `coder` and `ops` SHALL be created and the report SHALL list `old` as skipped with reason `tombstoned`

#### Scenario: Reserved name needs a target
- **GIVEN** a Hermes profile named `tools`
- **WHEN** it is migrated without `--into-profile`
- **THEN** that profile SHALL fail with `profile name 'tools' is reserved in Prometheus; use --into-profile` and other profiles SHALL proceed

### Requirement: Dry run writes nothing
The system SHALL, under `--dry-run`, compute the complete plan (every item and its disposition, counts per component, conflicts and the target paths) and print the report, SHALL NOT create the Prometheus home, any profile directory, the ledger or any secret, and SHALL exit 0 when the plan has no blocking error.
**Milestone:** M2

#### Scenario: Dry run on a fresh machine
- **GIVEN** no Prometheus home exists
- **WHEN** the user runs `prometheus migrate from-hermes --dry-run --all`
- **THEN** the report SHALL list planned imports per profile and the Prometheus home SHALL still not exist

### Requirement: Mapping report
The system SHALL produce a report assigning every Hermes config key, `.env` entry, credential file, skill, cron job, MCP server and plugin exactly one disposition (`mapped`, `transformed`, `default_omitted`, `secret`, `unsupported`, `ambiguous`, `conflict`, `skipped`, `failed`) with the Prometheus target and a reason, SHALL contain secret names but never secret values or message content, SHALL print a human summary (or the full report under `--json`), and on a real run SHALL save it to `<profile>/migrations/hermes-<run-id>.json`.
**Milestone:** M2

#### Scenario: Unsupported key reported
- **GIVEN** a Hermes `config.yaml` with `human_delay.mode: natural`
- **WHEN** the migration runs
- **THEN** the report SHALL list `human_delay.mode` with disposition `unsupported` and no Prometheus key SHALL be written for it

#### Scenario: No secret values in report
- **GIVEN** a Hermes `.env` with `OPENAI_API_KEY=sk-abc`
- **WHEN** the report is written
- **THEN** it SHALL contain `OPENAI_API_KEY` and SHALL NOT contain `sk-abc`

### Requirement: Config translation from user-set values only
The system SHALL translate only keys present in the Hermes profile's own `config.yaml` (never Hermes built-in defaults or the `/etc/hermes` managed layer, which is reported), SHALL map each key through a declarative mapping table to a Prometheus `config.toml` key with an optional value transform, SHALL omit a value equal to the Hermes default as `default_omitted` so Prometheus defaults apply, SHALL move root `max_turns` to `agent.max_iterations`, SHALL map the Hermes `agent.personality` selection into the profile identity file `SOUL.md` (never a config key), and SHALL validate the resulting `config.toml` against the Prometheus schema before writing it.
**Milestone:** M2

#### Scenario: Model settings mapped
- **GIVEN** Hermes `config.yaml` with `model.default: claude-sonnet-4` and `model.provider: anthropic`
- **WHEN** the config component migrates
- **THEN** the Prometheus `config.toml` SHALL contain `[model]` with `default = "claude-sonnet-4"` and `provider = "anthropic"`

#### Scenario: Default value not carried
- **GIVEN** Hermes `config.yaml` explicitly setting `tool_output.max_bytes: 50000`, equal to the Hermes default
- **WHEN** the config component migrates
- **THEN** no `tool_output.max_bytes` equivalent SHALL be written and the report SHALL show `default_omitted`

### Requirement: Supported Hermes config versions
The system SHALL accept Hermes `_config_version` 12 through 52, SHALL migrate a newer version best-effort with the WARN `Hermes config version <n> is newer than this migrator knows (52); unknown keys will be reported`, and SHALL refuse the config component for a version below 12 with `Hermes config predates version 12; run hermes update first`.
**Milestone:** M2

#### Scenario: Too old
- **GIVEN** a Hermes `config.yaml` with `_config_version: 9`
- **WHEN** the migration runs
- **THEN** the config component SHALL fail with the predates message and the other components SHALL proceed

### Requirement: Ambiguous YAML values resolve fail-closed
The system SHALL detect YAML 1.1 boolean literals (`on`, `off`, `yes`, `no`, unquoted) and other type coercions landing on keys that are enums in Prometheus, SHALL map such a value on an approval, sandbox, redaction or network-security key to the most restrictive Prometheus value with disposition `ambiguous` and a review hint, and SHALL map it on any other enum key to the value Hermes applied at runtime.
**Milestone:** M2

#### Scenario: Unquoted off on cron approvals
- **GIVEN** Hermes `config.yaml` containing `approvals.cron_mode: off` (unquoted, parsed by Hermes as boolean false)
- **WHEN** the config component migrates
- **THEN** the Prometheus value SHALL be the deny-by-default mode and the report SHALL mark the key `ambiguous` with the hint to set it explicitly

### Requirement: Config references to environment values
The system SHALL convert a Hermes `${NAME}` or `${env:NAME}` reference in a config value to a Prometheus secret reference `{ secret = "NAME" }` when `NAME` is classified as a secret, SHALL inline the value from the Hermes `.env` with disposition `transformed` when `NAME` is a non-secret, and SHALL report unresolved references and non-env sources (`${file:...}`, vault sources) as `unsupported`.
**Milestone:** M2

#### Scenario: Secret reference in MCP header
- **GIVEN** an MCP server header `Authorization: Bearer ${GITHUB_TOKEN}`
- **WHEN** it is migrated
- **THEN** the Prometheus header SHALL reference secret `GITHUB_TOKEN` and the secrets store SHALL hold its value

### Requirement: `.env` re-sorted into secrets and config
The system SHALL parse the Hermes profile `.env` file (never the shell environment) and classify each entry as: a secret (names registered as credentials by Hermes or Prometheus providers, adapters and tools, or matching `*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `*_KEY`) stored under the same name in the profile secrets store; a known non-secret behaviour variable mapped to its config key through the mapping table (including `MESSAGING_CWD`/`TERMINAL_CWD` to `terminal.cwd`, `HERMES_TIMEZONE` to `cron.timezone`, `HERMES_CRON_MAX_PARALLEL` to `cron.max_parallel_jobs`, `HERMES_GATEWAY_BUSY_INPUT_MODE` to `busy.input_mode`; a known variable with no Prometheus key, such as `HERMES_SEARCH_SLOW_MS`, reported as `unsupported`); a process-control variable (`HERMES_HOME`, `HERMES_PROFILE`, `PATH`, `PYTHONPATH`, `HERMES_CONFIG_BACKEND` and similar) dropped as `skipped`; or, when unrecognised, a secret with disposition `secret` and reason `unclassified`.
**Milestone:** M2

#### Scenario: Non-secret moved into config
- **GIVEN** a Hermes `.env` containing `HERMES_CRON_MAX_PARALLEL=4` and `ANTHROPIC_API_KEY=...`
- **WHEN** the secrets component migrates
- **THEN** `config.toml` SHALL contain `cron.max_parallel_jobs = 4`, the secrets store SHALL contain `ANTHROPIC_API_KEY`, and the secrets store SHALL NOT contain `HERMES_CRON_MAX_PARALLEL`

#### Scenario: Unknown name treated as secret
- **GIVEN** a Hermes `.env` containing `ACME_WIDGET=xyz`
- **WHEN** the secrets component migrates
- **THEN** `ACME_WIDGET` SHALL be stored as a secret and reported with reason `unclassified`

### Requirement: Credential files imported
The system SHALL import Hermes credential files that map to a supported Prometheus provider or adapter (`auth.json` credential pools and OAuth tokens, `.anthropic_oauth.json`, Google and Slack token files) into the profile secrets store with their expiry metadata, SHALL report each unsupported credential file as `unsupported` without copying it, and SHALL never copy a credential file verbatim into the Prometheus profile directory.
**Milestone:** M2

#### Scenario: Credential pool preserved
- **GIVEN** a Hermes `auth.json` with two OpenRouter keys in a credential pool
- **WHEN** the secrets component migrates
- **THEN** the Prometheus profile SHALL have both keys in the secrets store and `config.toml` SHALL declare the OpenRouter credential pool referencing both by secret name

### Requirement: Typed names validated against Prometheus registries
The system SHALL translate Hermes toolset, provider, theme (skin) and platform names through mapping tables to Prometheus registry names, SHALL drop and report as `unsupported` any name with no Prometheus equivalent, and SHALL never write a name to `config.toml` that the Prometheus registries would reject.
**Milestone:** M2

#### Scenario: Unknown skin falls back
- **GIVEN** Hermes `display.skin: charizard`
- **WHEN** the config component migrates
- **THEN** `display.theme` SHALL be omitted (default applies) and the report SHALL list `display.skin` as `unsupported` with value `charizard`

### Requirement: Sessions imported with lineage
The system SHALL import Hermes sessions from the staged `state.db` (schema versions 24 through 32) with title and title provenance, source, origin (platform, chat, thread, user), model, cwd, git metadata, timestamps, end reason, pinned, archived and hidden flags, and every message with role, content, tool calls, reasoning, timestamps and the `active`/`compacted` flags, SHALL translate Hermes parent links into typed lineage (`compression` when the parent ended with reason `compression` and the child is not an explicit fork, `delegate` for delegate-marked children, `fork` for branches, `reset` for reset children), SHALL keep the Hermes id as `legacy_id` so `--resume <hermes-id>` resolves, and SHALL NOT import Hermes runtime coordination tables (turn leases, compression locks, admissions, worker executions and receipts, gateway routing, async delegations).
**Milestone:** M2

#### Scenario: Compression chain preserved
- **GIVEN** Hermes sessions A then B where A ended with `end_reason = 'compression'` and B's parent is A
- **WHEN** sessions migrate
- **THEN** the Prometheus copy of B SHALL have `lineage_kind = "compression"` with the copy of A as parent, and `lineage(A)` SHALL return both

#### Scenario: Resume by Hermes id
- **GIVEN** a migrated Hermes session `20260301_101500_a1b2c3`
- **WHEN** the user runs `prometheus --resume 20260301_101500_a1b2c3`
- **THEN** the corresponding Prometheus session SHALL resume

#### Scenario: Unsupported schema
- **GIVEN** a Hermes `state.db` at schema version 18
- **WHEN** sessions migrate
- **THEN** the sessions component SHALL fail with `Hermes state.db schema 18 is not supported (24-32)` and the other components SHALL proceed

### Requirement: Imported usage carried as events
The system SHALL carry each Hermes session's token totals and cost into one usage event per session and per model (from Hermes per-model usage rows when present) with cost source `hermes_import`, so lineage and profile totals in Prometheus include the imported history.
**Milestone:** M2

#### Scenario: Profile totals include Hermes history
- **GIVEN** Hermes sessions totalling 1.2M input tokens
- **WHEN** sessions migrate and the user runs `prometheus sessions stats --profile-total`
- **THEN** the input token total SHALL be at least 1.2M

### Requirement: Session import is streaming, verified and indexed
The system SHALL import sessions in transactions of at most 500 sessions, report progress to stderr, verify per-session message counts against the source after commit (recording a mismatch as `failed` for that session), move provider-specific opaque fields (for example Codex reasoning items) into the message's opaque provider payload, and rebuild search indexes for the imported rows before reporting success.
**Milestone:** M2

#### Scenario: Imported messages searchable
- **GIVEN** a Hermes message containing `flamegraph`
- **WHEN** sessions migrate
- **THEN** `prometheus sessions search flamegraph` SHALL return the imported message

### Requirement: Memory and identity imported
The system SHALL import `memories/MEMORY.md` and `memories/USER.md` by splitting on the Hermes entry delimiter `\n§\n` into ordered memory entries of the Prometheus agent and user stores respectively, SHALL import every entry even when the total exceeds the Prometheus memory budget (reporting `over budget; run prometheus memory curate`), SHALL copy `SOUL.md` to the profile identity file, and SHALL report external memory provider configuration (for example Honcho or Mem0) as `unsupported` unless a matching Prometheus memory provider plugin is installed.
**Milestone:** M2

#### Scenario: Entries preserved in order
- **GIVEN** a Hermes `MEMORY.md` with three entries separated by `§`
- **WHEN** memory migrates
- **THEN** the Prometheus agent memory SHALL contain the same three entries in the same order with identical text

### Requirement: Skills imported with scanning
The system SHALL copy user-authored and hub-installed skill directories from the Hermes profile `skills/` into `<profile>/skills/` preserving structure and hub provenance, SHALL skip Hermes bundled skills listed in `skills/.bundled_manifest` whose content hash is unchanged (Prometheus ships its own), SHALL run the Prometheus skill security scan on every imported skill and place a failing or unscannable skill under `<profile>/skills/.quarantine/` disabled, SHALL mark skills containing Python scripts as requiring the Python bridge, and SHALL carry the disabled-skills list into `skills.disabled`.
**Milestone:** M2

#### Scenario: Modified bundled skill kept
- **GIVEN** a Hermes bundled skill whose files the user edited (hash differs from the manifest)
- **WHEN** skills migrate
- **THEN** the skill SHALL be imported as a user skill and reported as `transformed` with reason `locally modified bundled skill`

#### Scenario: Scan failure quarantined
- **GIVEN** an imported skill whose script exfiltrates `~/.ssh`
- **WHEN** skills migrate
- **THEN** the skill SHALL be under `skills/.quarantine/`, disabled, and reported `failed` with the scan finding

### Requirement: Scheduled jobs imported paused
The system SHALL import each Hermes job from `cron/jobs.json` (id, name, prompt, skills, script, schedule, repeat, delivery target, model and provider overrides, toolsets, grace window, enabled and paused state), SHALL preserve Hermes schedule semantics while emitting only the Prometheus grammar of `scheduled-jobs` (a Hermes bare duration such as `30m`, which Hermes treats as a recurring interval and Prometheus rejects as ambiguous, SHALL be rewritten to `every 30m`; `in 30m` and an ISO timestamp stay one-shots; cron expressions unchanged), SHALL copy referenced scripts from the Hermes `scripts/` into `<profile>/scripts/`, SHALL skip completed one-shot jobs, and SHALL import every job paused unless `--enable-cron` is given so the same job cannot fire from both Hermes and Prometheus.
**Milestone:** M2

#### Scenario: Bare interval stays recurring
- **GIVEN** a Hermes job with schedule `30m`
- **WHEN** cron migrates
- **THEN** the Prometheus job SHALL have schedule `every 30m` (kind `interval`), state `paused`, and the report SHALL record the rewrite as `transformed`

#### Scenario: Enabled with opt-in
- **WHEN** the user runs the migration with `--enable-cron`
- **THEN** jobs enabled in Hermes SHALL be enabled in Prometheus and the summary SHALL warn to stop the Hermes scheduler

### Requirement: MCP servers and plugins
The system SHALL import Hermes `mcp_servers` definitions into Prometheus MCP server configuration with credential-bearing environment values and headers moved into the secrets store, SHALL inventory Hermes plugins and report each as `bridge-compatible` (imported disabled for the Python bridge with no capability grants) or `unsupported`, and SHALL never enable a plugin or grant it capabilities during migration.
**Milestone:** M2

#### Scenario: Plugin imported disabled
- **GIVEN** a Hermes plugin that the bridge supports
- **WHEN** plugins migrate
- **THEN** the plugin SHALL be listed by `prometheus plugins list` as disabled with no grants, and the report SHALL tell the user to run `prometheus plugins enable <name>` to review its capabilities

### Requirement: Gateway settings and pairing allowlists
The system SHALL import messaging platform settings for adapters Prometheus supports (tokens to secrets, allowlists, home channels, busy and streaming settings to config) and approved pairings to the gateway authorization store of the same profile, and SHALL report settings for unsupported platforms as `unsupported`.
**Milestone:** M2

#### Scenario: Approved Telegram user stays approved
- **GIVEN** a Telegram user approved through Hermes pairing
- **WHEN** the gateway component migrates
- **THEN** that user SHALL be authorized in the Prometheus gateway for the same profile without re-pairing

### Requirement: Idempotent incremental re-run
The system SHALL record in a migration ledger (`<profile>/migrations/hermes-ledger.toml`) the source home path and identity, each imported item's source key and content hash and the value written, SHALL on re-run import only new or changed items, SHALL update a destination value only when it still equals what the previous run wrote, and otherwise SHALL keep the destination value and report `conflict` unless `--overwrite` is given.
**Milestone:** M2

#### Scenario: Re-run imports only new sessions
- **GIVEN** a completed migration and 3 new Hermes sessions since
- **WHEN** the migration runs again
- **THEN** exactly 3 sessions SHALL be imported and no existing Prometheus session SHALL be duplicated

#### Scenario: User edit preserved
- **GIVEN** a migrated `agent.max_iterations = 60` that the user later changed to 80 in Prometheus, and Hermes now has 90
- **WHEN** the migration runs again without `--overwrite`
- **THEN** `agent.max_iterations` SHALL remain 80 and the report SHALL list a `conflict`

#### Scenario: Grown Hermes session appended
- **GIVEN** a migrated Hermes session that received 4 more messages in Hermes and was not continued in Prometheus
- **WHEN** the migration runs again
- **THEN** the 4 messages SHALL be appended to the existing Prometheus session

### Requirement: Per-component atomicity and resumability
The system SHALL stage each component's output and commit it per profile and component, so an interruption or a failing component leaves every committed component complete and recorded in the ledger and every uncommitted component absent, and a later run SHALL resume from the ledger.
**Milestone:** M2

#### Scenario: Interrupted during sessions
- **GIVEN** a migration interrupted with Ctrl+C while importing sessions
- **WHEN** the user re-runs the same command
- **THEN** config, secrets and memory SHALL not be re-imported and the session import SHALL continue from the last committed batch

### Requirement: Migration exit codes and confirmation
The system SHALL show the plan summary and ask for confirmation before writing unless `--yes` is given or stdin is not a terminal (in which case it SHALL require `--yes` and exit 4 without it), and SHALL exit 0 when every selected component committed (unsupported and conflict entries do not fail the run), 1 when any component failed, 66 when the source is missing, and 73 when the destination cannot be written.
**Milestone:** M2

#### Scenario: Non-interactive without --yes
- **WHEN** the migration runs from a script without `--yes`
- **THEN** it SHALL exit 4 with `refusing to migrate non-interactively without --yes` and write nothing

### Requirement: Migration rollback (upgrade)
The system SHALL provide `prometheus migrate rollback --run <run-id>` that removes every item the run created and restores every value the run overwrote, using the ledger, while keeping any item modified in Prometheus after the run and reporting it as `kept_modified`.
**Milestone:** M3

#### Scenario: Rollback keeps a continued session
- **GIVEN** a migration run that imported 10 sessions, one of which the user continued in Prometheus
- **WHEN** the user rolls the run back
- **THEN** 9 sessions SHALL be removed and the continued one SHALL remain, reported `kept_modified`

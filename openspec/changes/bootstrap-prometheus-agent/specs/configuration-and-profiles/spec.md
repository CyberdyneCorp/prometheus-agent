# Configuration and Profiles

## Purpose
Configuration and profiles define where Prometheus keeps state, how settings and secrets are typed, layered, validated, migrated and written, and how profiles act as isolated islands that one process can serve side by side. The key decisions are: one typed `config.toml` per profile whose schema is the single source of defaults (docs, completions and `config explain` are generated from it), secrets in a separate store and the only values that may come from the environment, and an explicit `ProfileScope` value carried by every operation so no code path can read another profile's state by accident.

## ADDED Requirements

### Requirement: Home resolution
The system SHALL resolve the Prometheus home once at process start as the `--home PATH` flag if given, else a non-blank `PROMETHEUS_HOME` (with `~` expanded), else `~/.prometheus` on macOS and Linux and `%LOCALAPPDATA%\Prometheus` on Windows, SHALL create it with mode `0700` when absent, and SHALL NOT re-read the environment for the home afterwards.
**Milestone:** M1

#### Scenario: Environment override
- **GIVEN** `PROMETHEUS_HOME=/srv/pmt` and no `--home` flag
- **WHEN** any `prometheus` command starts
- **THEN** every profile path SHALL be under `/srv/pmt/profiles/`

#### Scenario: Flag wins over environment
- **GIVEN** `PROMETHEUS_HOME=/srv/pmt`
- **WHEN** the user runs `prometheus --home /tmp/h config path`
- **THEN** the printed path SHALL be under `/tmp/h`

### Requirement: Home and profile layout
The system SHALL lay out the home as in design decision D13: the root holds only `<home>/active_profile`, `<home>/daemon.toml` (daemon-wide settings: listeners, ports, relay, all-adapters-down policy, and the machine-wide sections `runtimes.*`, `update.*`, `process.*` and `[local_runtime]`, which SHALL be refused in a profile `config.toml`), `<home>/run/` (daemon runtime records and sockets), `<home>/logs/` (daemon, update and migration logs), `<home>/runtimes/` (hash-pinned managed runtimes), `<home>/updates/` (install and update receipts), `<home>/kanban/` (boards shared across profiles) and `<home>/profiles/<name>/` for every profile including `default`; each profile directory SHALL contain `config.toml`, `secrets.toml` (when the file backend is used), `grants.toml`, `state.db`, and the directories `blobs/`, `skills/`, `harness/`, `trajectories/`, `cache/`, `scripts/`, `snapshots/`, `python/`, `memories/`, `cron/`, `logs/`, `themes/`, `plugins/`, `backups/` and `migrations/`, created on first use with mode `0700`.
**Milestone:** M1

#### Scenario: Default profile is a regular profile directory
- **WHEN** a fresh home is initialised
- **THEN** the default profile's configuration SHALL be at `<home>/profiles/default/config.toml` and no `config.toml` SHALL exist at the home root

### Requirement: Explicit ProfileScope
The system SHALL bind every operation that reads or writes profile data to an explicit `ProfileScope` value (profile name, profile directory, immutable config snapshot, secrets handle) created by the entry point, a gateway route, a scheduled job or an RPC handler, and SHALL provide no API that resolves profile paths, config or secrets from process-global state.
**Milestone:** M1

#### Scenario: Deferred callback keeps its profile
- **GIVEN** one gateway process serving profiles `work` and `home`
- **WHEN** a timer scheduled by a `home` session fires after a `work` turn has started
- **THEN** the callback SHALL read `home` config and secrets and write to `profiles/home/`

### Requirement: Environment is secrets-only
The system SHALL read from the process environment only `PROMETHEUS_HOME`, `PMT_LOG`/`RUST_LOG`, `PMT_SECRET_<NAME>` variables, provider-native credential names declared by registered providers (for example `OPENAI_API_KEY`), and standard terminal and operating-system variables (`HOME`, `USERPROFILE`, `LOCALAPPDATA`, `XDG_*`, `TERM`, `COLUMNS`, `NO_COLOR`, `TZ`, `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`), and SHALL log one WARN per process for each other set variable with the `PROMETHEUS_` or `PMT_` prefix stating that it is ignored and naming the config key that replaces it when one exists.
**Milestone:** M1

#### Scenario: Behaviour env var ignored
- **GIVEN** `PMT_MAX_TURNS=5` in the environment
- **WHEN** a session starts
- **THEN** the turn limit SHALL come from `config.toml` and a WARN SHALL say `PMT_MAX_TURNS is ignored; set agent.max_iterations in config.toml`

### Requirement: Typed configuration schema is the single source of defaults
The system SHALL define every configuration key once in a typed schema carrying type, default, allowed values or range, description, `apply` class (`live`, `next_session`, `restart`) and whether it is a secret reference, and SHALL derive the effective defaults, `prometheus config schema` (JSON Schema draft 2020-12), the generated configuration reference documentation and shell completions from that schema.
**Milestone:** M1

#### Scenario: Schema export
- **WHEN** the user runs `prometheus config schema`
- **THEN** stdout SHALL be a JSON Schema document containing a property for every key with its `default` and `description`

### Requirement: Strict validation
The system SHALL validate `config.toml` against the schema on every load, SHALL reject unknown keys (with a `did you mean` suggestion at edit distance 2 or less), wrong types, out-of-range values and unknown enum values, and SHALL report each error with file path, line and column; a file that fails to parse or validate at process start SHALL cause commands that need configuration to exit with code 78 and print every error.
**Milestone:** M1

#### Scenario: Unknown enum value
- **GIVEN** `config.toml` containing `[approvals]` with `mode = "of"`
- **WHEN** `prometheus chat` starts
- **THEN** the process SHALL exit 78 with an error naming `approvals.mode`, the line number and the allowed values

#### Scenario: Misspelled key
- **GIVEN** `config.toml` containing `[agent]` with `max_iteration = 40`
- **WHEN** the config is loaded
- **THEN** the error SHALL include `unknown key 'agent.max_iteration' (did you mean 'agent.max_iterations'?)`

### Requirement: Configuration layering
The system SHALL compute the effective configuration as, from lowest to highest precedence: schema defaults, the profile `config.toml`, per-invocation overrides given with `--set key=value` (never persisted), and the managed layer, and SHALL record for every effective value which layer supplied it.
**Milestone:** M1

#### Scenario: Per-invocation override not persisted
- **WHEN** the user runs `prometheus --set agent.max_iterations=10 chat` and then `prometheus config get agent.max_iterations`
- **THEN** the first session SHALL use 10 and the `config get` SHALL print the value from `config.toml` or the default

### Requirement: Config explain (upgrade)
The system SHALL provide `prometheus config explain <key>` printing the key's type, default, effective value, supplying layer and file location, description, `apply` class and, for a managed key, that it is locked, and `prometheus config explain --changed` listing every key whose effective value differs from its default.
**Milestone:** M1

#### Scenario: Explain shows provenance
- **GIVEN** `agent.max_iterations = 40` in the profile `config.toml` at line 12
- **WHEN** the user runs `prometheus config explain agent.max_iterations`
- **THEN** the output SHALL show the default, the effective value 40, the source `config.toml:12` and the apply class `next_session`

### Requirement: Format-preserving atomic config writes
The system SHALL write configuration only through one writer that validates the full resulting document against the schema before writing, preserves comments, ordering and untouched keys, writes via temp file plus `fsync` plus atomic rename with mode `0600`, keeps the previous 10 versions under `<profile>/backups/config/`, and refuses to write when the on-disk file fails to parse.
**Milestone:** M1

#### Scenario: Comment preserved
- **GIVEN** a `config.toml` with a comment above `[model]`
- **WHEN** the user runs `prometheus config set display.theme mono`
- **THEN** the comment and every other key SHALL remain byte-identical and `display.theme` SHALL be `"mono"`

#### Scenario: Invalid value never written
- **WHEN** the user runs `prometheus config set agent.max_iterations -3`
- **THEN** the command SHALL exit 2 with a range error and the file SHALL be unchanged

### Requirement: Config CLI
The system SHALL provide `prometheus config` subcommands `path`, `show [--effective] [--json]`, `get <key>`, `set <key> <value>`, `unset <key>`, `edit` (opens `$EDITOR`, then validates and re-opens on error until fixed or aborted), `validate` (exit 0 valid, 78 invalid), `explain`, `schema` and `docs`, where `unset` removes the key so the default applies.
**Milestone:** M1

#### Scenario: Edit loop rejects invalid result
- **GIVEN** the user saves an invalid `config.toml` in the editor launched by `prometheus config edit`
- **WHEN** the editor exits
- **THEN** the system SHALL print the validation errors and offer to re-open the editor, keeping the last valid file in place if the user aborts

### Requirement: Secrets are not configuration
The system SHALL refuse to store a value in a non-secret config key when it matches a known credential pattern (for example `sk-`, `ghp_`, `xoxb-`, `AKIA` prefixes or a PEM private key header), failing with `this looks like a credential; store it with: prometheus secrets set <NAME>` (a command-line value exits 2; a credential found in `config.toml` is a validation error, exit 78, for `prometheus config validate`, `config edit` and every command that loads the profile), and config keys that need a credential SHALL accept only a secret reference: a key named `<purpose>_secret` (or `<purpose>_secrets` for a list) whose value is the secret's name, for example `providers.acme.key_secret = "ACME_API_KEY"`, never the value itself.
**Milestone:** M1

#### Scenario: API key pasted into config
- **WHEN** the user runs `prometheus config set providers.acme.base_url sk-live-123456`
- **THEN** the command SHALL fail with the credential message and SHALL NOT write the value

#### Scenario: Credential written into the file by hand
- **GIVEN** a profile `config.toml` containing `base_url = "sk-live-123456"` under `[providers.acme]`
- **WHEN** the user runs `prometheus config validate` or any command that loads the profile
- **THEN** the command SHALL exit 78 with the credential message naming `providers.acme.base_url` and its line, without printing the value

### Requirement: Secrets store
The system SHALL keep each profile's secrets in a store selected by `secrets.backend` (`file` default: `<profile>/secrets.toml` with mode `0600`; `keychain`: the OS keychain under service `prometheus` and account `<home-id>/<profile>/<NAME>`), SHALL restrict secret names to `^[A-Z][A-Z0-9_]{0,127}$`, and SHALL provide `prometheus secrets set <NAME>` (value read from a no-echo prompt or stdin, never from argv), `list` (names, source and whether shadowed, never values), `get <NAME> --reveal` (TTY only), `unset <NAME>` and `migrate --to <backend>`.
**Milestone:** M1

#### Scenario: Value never on argv
- **WHEN** the user runs `echo -n $TOKEN | prometheus secrets set TELEGRAM_BOT_TOKEN`
- **THEN** the secret SHALL be stored and `prometheus secrets list` SHALL show `TELEGRAM_BOT_TOKEN` without its value

#### Scenario: Reveal refused without TTY
- **WHEN** `prometheus secrets get OPENAI_API_KEY --reveal` runs with stdout piped
- **THEN** the command SHALL exit 77 and print nothing to stdout

### Requirement: Secret resolution precedence and scope
The system SHALL resolve a secret for a `ProfileScope` from that profile's secrets store first and then, only when the scope is the process's launch profile, from `PMT_SECRET_<NAME>` or the provider-native environment variable, SHALL never resolve an environment-supplied secret for any other profile served by the same process, and SHALL report a missing secret as error `secret.missing` naming the secret and the `prometheus secrets set` command.
**Milestone:** M1

#### Scenario: Launch environment does not leak to another profile
- **GIVEN** a gateway launched for profile `default` with `OPENAI_API_KEY` in its environment, also serving profile `home` whose store has no `OPENAI_API_KEY`
- **WHEN** a `home` session needs `OPENAI_API_KEY`
- **THEN** resolution SHALL fail with `secret.missing` for profile `home`

### Requirement: Config schema versioning and migration
The system SHALL store `schema_version` at the top of `config.toml`, SHALL apply forward migrations at load time when the file is older (after copying it to `<profile>/backups/config/config-v<old>-<timestamp>.toml`), persisting only values that differ from the new defaults, and SHALL refuse to load a file whose `schema_version` is newer than the binary supports, exiting 78 with `config.toml schema_version <n> is newer than this prometheus supports (<m>); upgrade prometheus`.
**Milestone:** M1

#### Scenario: Renamed key migrated
- **GIVEN** a version-1 file containing a key renamed in version 2
- **WHEN** a version-2 binary loads it
- **THEN** the file SHALL contain the new key name with the same value, `schema_version = 2`, and a backup of the version-1 file

### Requirement: Live reload with apply classes
The system SHALL, in a long-running process, watch each served profile's `config.toml`, validate a changed file before using it, keep the previous snapshot and log an ERROR (also surfaced by `prometheus doctor` and the gateway status) when it is invalid, and otherwise apply `live` keys immediately, `next_session` keys only to sessions created afterwards, and mark `restart` keys as pending restart; a running session SHALL keep the config snapshot it started with for every `next_session` key.
**Milestone:** M2

#### Scenario: Prompt-affecting key deferred
- **GIVEN** an active gateway session and a change to `prompt.load_identity` (apply class `next_session`)
- **WHEN** the file is saved
- **THEN** the active session's system prompt SHALL be unchanged and the next new session SHALL use the new identity setting

#### Scenario: Invalid edit keeps the daemon running
- **WHEN** the user saves `config.toml` with a syntax error while the gateway runs
- **THEN** the gateway SHALL keep serving with the previous snapshot and `prometheus daemon status` SHALL show the config error

### Requirement: Managed configuration layer
The system SHALL load an administrator-managed layer from `/etc/prometheus/managed.toml` (`%ProgramData%\Prometheus\managed.toml` on Windows) when present, validated with the same schema, whose values override the user's and whose keys listed in `locked = [...]` SHALL be refused by every writer with `config key '<key>' is managed by your administrator`.
**Milestone:** M2

#### Scenario: Locked key refused
- **GIVEN** a managed layer locking `approvals.mode`
- **WHEN** the user runs `prometheus config set approvals.mode off`
- **THEN** the command SHALL exit 77 with the managed-key message

### Requirement: Profile name rules
The system SHALL normalise profile names to lowercase, SHALL require `^[a-z0-9][a-z0-9_-]{0,63}$`, and SHALL reject the reserved names `prometheus`, `pmt`, `test`, `tmp`, `root`, `all`, `none`, `run`, `logs` and `tools` for new profiles with `profile name '<name>' is reserved`.
**Milestone:** M1

#### Scenario: Invalid name
- **WHEN** the user runs `prometheus profile create ../evil`
- **THEN** the command SHALL exit 2 with `invalid profile name` and create nothing

### Requirement: Profiles are islands
The system SHALL NOT let any profile read another profile's configuration, secrets, sessions, memory, skills, plugins or scheduled jobs at runtime, and SHALL share between profiles only the home-level managed runtimes in `<home>/runtimes/`, the daemon settings in `<home>/daemon.toml`, the daemon runtime records in `<home>/run/`, the update receipts in `<home>/updates/` and the kanban boards in `<home>/kanban/` (shared on purpose).
**Milestone:** M1

#### Scenario: Skill installed in one profile only
- **GIVEN** a skill installed into profile `work`
- **WHEN** a session in profile `home` lists skills
- **THEN** the skill SHALL NOT be listed

### Requirement: Profile lifecycle commands
The system SHALL provide `prometheus profile list`, `create <name> [--clone-from <name>] [--clone-secrets]`, `use <name>`, `show [<name>]`, `rename <old> <new>`, `delete <name> [--yes]`, `export <name> -o FILE` and `import FILE [--as <name>]`, where `--clone-from` copies config, identity file, memory, skills, themes and plugin installations once (no runtime inheritance) and copies secrets only with `--clone-secrets`, and creating an existing profile SHALL fail with `profile '<name>' already exists`.
**Milestone:** M1

#### Scenario: Clone without secrets
- **WHEN** the user runs `prometheus profile create coder --clone-from default`
- **THEN** `profiles/coder/config.toml` SHALL equal the default profile's and `profiles/coder` SHALL contain no secrets

### Requirement: Safe profile deletion
The system SHALL refuse to delete `default`, SHALL require typing the profile name unless `--yes`, SHALL stop the profile's scheduled jobs and gateway routes before removal, SHALL write a tombstone `<home>/profiles/.deleted/<name>` before removing the directory so a concurrent process cannot recreate it, and SHALL clear `active_profile` when it named the deleted profile.
**Milestone:** M1

#### Scenario: Concurrent writer cannot resurrect
- **GIVEN** a gateway serving profile `old`
- **WHEN** `prometheus profile delete old --yes` runs
- **THEN** after completion `profiles/old/` SHALL not exist and any later attempt by the gateway to write there SHALL fail with `profile.deleted`

### Requirement: Profile selection at process start
The system SHALL select the launch profile from, in order, `-p/--profile <name>`, else the name in `<home>/active_profile`, else `default`, SHALL fail with exit 2 and `profile '<name>' does not exist; create it with: prometheus profile create <name>` for an unknown name, and `prometheus profile use <name>` SHALL write `active_profile` atomically (`use default` removes the file).
**Milestone:** M1

#### Scenario: Flag beats sticky selection
- **GIVEN** `active_profile` contains `work`
- **WHEN** the user runs `prometheus -p home chat`
- **THEN** the session SHALL run in profile `home`

### Requirement: Profile alias launchers
The system SHALL, on `prometheus profile alias <name> [--as <alias>]`, write a launcher `<alias>` (default the profile name) into the user's bin directory (`~/.local/bin` on POSIX, `%LOCALAPPDATA%\Prometheus\bin` on Windows) that runs `prometheus -p <name> "$@"`, and SHALL refuse an alias that is a reserved name, a `prometheus` subcommand name, or an existing executable on `PATH`.
**Milestone:** M2

#### Scenario: Alias collision
- **GIVEN** an executable `git` on `PATH`
- **WHEN** the user runs `prometheus profile alias work --as git`
- **THEN** the command SHALL exit 2 naming the existing executable

### Requirement: Served profile set
The system SHALL serve, in a gateway daemon, the launch profile only by default, and with `gateway.multiplex = true` in the launch profile every live profile except those with `gateway.standalone = true`, creating a separate `ProfileScope` per served profile and never creating profile directories while computing the set.
**Milestone:** M2

#### Scenario: Standalone profile excluded
- **GIVEN** multiplexing enabled and profile `lab` with `gateway.standalone = true`
- **WHEN** the gateway starts
- **THEN** `lab` SHALL NOT be served

### Requirement: Profile export excludes credentials by default
The system SHALL export a profile as a `.tar.zst` archive containing config, identity file, memory, skills, themes, scripts and cron job definitions, SHALL exclude secrets, `state.db`, blobs, logs and backups by default, SHALL include sessions only with `--with-sessions`, and SHALL include secrets only with `--with-secrets`, encrypting them with a passphrase-derived key (Argon2id plus XChaCha20-Poly1305) entered at a no-echo prompt.
**Milestone:** M2

#### Scenario: Default export has no secrets
- **WHEN** the user runs `prometheus profile export work -o work.tar.zst`
- **THEN** the archive SHALL contain no `secrets.toml`, no keychain material and no `state.db`

### Requirement: Whole-home backup
The system SHALL provide `prometheus backup create [-o <file>] [--quick] [--label <text>] [--keep <n>]` producing a `.tar.zst` archive of the whole home (every profile plus `active_profile` and `daemon.toml`) with a manifest of member paths, sizes and SHA-256 hashes, copying each `state.db` through the SQLite online backup API (never the raw file or its `-wal`/`-shm` sidecars), always excluding `runtimes/`, `run/`, `logs/`, `cache/` directories, browser profiles, sockets, lock files and previous backups, writing to `<home>/backups/prometheus-<UTC timestamp>[-<label>].tar.zst` by default, and pruning older default-named backups beyond `--keep` (default 3); `--quick` SHALL include only critical state per profile (`config.toml`, `secrets.toml`, `grants.toml`, `state.db`, cron job definitions, pairing and allowlist data, the projects store) and the home-level files; a second concurrent backup SHALL exit 75 with `another backup is running`, and a backup that had to skip unreadable members SHALL still write the archive, list the skipped members and exit 5.
**Milestone:** M1

#### Scenario: Live database captured consistently
- **GIVEN** a running daemon writing to `profiles/default/state.db`
- **WHEN** the user runs `prometheus backup create`
- **THEN** the archive SHALL contain a `state.db` that passes an integrity check and no `state.db-wal` member

#### Scenario: Retention
- **GIVEN** three earlier default-named backups in `<home>/backups/`
- **WHEN** the user runs `prometheus backup create --keep 3`
- **THEN** `<home>/backups/` SHALL contain the three newest backups

### Requirement: Home restore
The system SHALL provide `prometheus backup restore <archive> [--profile <name>] [--force]` that verifies every member against the manifest hashes before writing anything (exit 65 and no change on any mismatch), refuses while a daemon or interactive client is running for an affected profile (exit 75 naming it), asks for confirmation when the target already holds data (exit 4 non-interactively without `--force`), warns `restoring an older backup: <n> sessions and <m> messages newer than the backup will be lost` when the replaced `state.db` has more sessions or later activity than the restored one, takes a quick snapshot of the replaced state first, restores `state.db` files through the SQLite backup API, never restores members from the exclusion list even if present, and with `--profile` restores only that profile's subtree.
**Milestone:** M1

#### Scenario: Corrupt archive writes nothing
- **GIVEN** an archive with one member whose hash differs from its manifest entry
- **WHEN** the user runs `prometheus backup restore b.tar.zst --force`
- **THEN** the command SHALL exit 65 and no file in the home SHALL change

### Requirement: State snapshots
The system SHALL provide quick per-profile state snapshots under `<profile>/snapshots/<id>/` (id `snap_<ULID>`, label, creation time, reason `manual`, `pre-update`, `pre-restore` or `pre-migration`) with the `--quick` member set, through `prometheus snapshots list|create [--label <text>]|restore <id>|prune [--keep <n>]` and the slash command `/snapshot [create [label]|list|restore <id>|prune [n]]` (alias `/snap`, surfaces `cli`, `tui`, `desktop`), keeping the newest `snapshots.keep` manual snapshots (default 20) while update snapshots follow `update.snapshot_keep`; `restore` SHALL take a `pre-restore` snapshot first, drain the profile's live sessions to a turn boundary and close the profile's store handles (refusing with exit 75 when a turn does not finish within 30 s), replace the files atomically and reopen the profile.
**Milestone:** M1

#### Scenario: Restore pre-update snapshot
- **GIVEN** a snapshot with reason `pre-update` taken before an update
- **WHEN** the user runs `prometheus snapshots restore <id>`
- **THEN** the profile's `config.toml` and `state.db` SHALL equal the snapshot's and a new `pre-restore` snapshot SHALL be listed

### Requirement: Managed secrets and managed visibility
The system SHALL load an administrator-managed secrets file `managed-secrets.toml` beside `managed.toml` (mode 0600 or 0640 owned by root) whose values take precedence over every profile store and environment value for the same name and cannot be unset or overwritten by `prometheus secrets` (`secret '<NAME>' is managed by your administrator`), SHALL allow relocating the managed directory only through the platform's machine-wide location (never a per-user setting or environment variable), SHALL list the managed source path and every managed or locked key in a header of `prometheus config show`, and SHALL have `prometheus doctor` report the resolved managed paths and whether they were loaded.
**Milestone:** M2

#### Scenario: Managed key shown
- **GIVEN** `managed.toml` sets and locks `approvals.mode`
- **WHEN** the user runs `prometheus config show`
- **THEN** the header SHALL name `/etc/prometheus/managed.toml` and list `approvals.mode (locked)`

### Requirement: Remote configuration backend
The system SHALL support `config_backend.kind = "remote"` set only in `managed.toml` or `<home>/daemon.toml` (default `file`), with `config_backend.url`, `config_backend.instance_id` (required) and a credential read from the secret named by `config_backend.token_secret`, under which each profile's configuration is fetched at process start, layered by the remote plane (global, tenant, group, owner, agent, profile) with locked keys, retried for up to 30 s and otherwise failing closed with exit 78 (never falling back to a local file or defaults), kept only in memory, polled every `config_backend.poll_secs` (default 300) with changes applied through the live-reload apply classes, and written back by every config writer as the changed keys only at the profile level with optimistic concurrency, refusing locked keys with the name of the level that locks them; credentials SHALL never be accepted as remote values.
**Milestone:** M3

#### Scenario: Unreachable plane fails closed
- **GIVEN** `config_backend.kind = "remote"` and an unreachable plane
- **WHEN** the user runs `prometheus chat`
- **THEN** the process SHALL exit 78 after about 30 s with a message naming the plane URL and SHALL NOT read `config.toml`

### Requirement: Profile creation options
`prometheus profile create` SHALL also accept `--clone-all` (copy everything from the source profile including plugins, cron definitions and themes but excluding sessions, `state.db`, logs, snapshots and backups), `--no-skills` (create without seeding bundled skills, setting `skills.include_bundled = false` in the new profile), `--sync-imports` (with a clone flag, also copy the import sync registry of `hermes-migration` › Keeping imports in sync), `--description <text>`, `--no-alias` (skip the default alias launcher), and `--clone-channels`, without which messaging adapter configuration, pairing and allowlist data SHALL never be cloned so two profiles do not answer the same bot account.
**Milestone:** M2

#### Scenario: Channels not cloned by default
- **GIVEN** profile `work` with a Telegram adapter configured
- **WHEN** the user runs `prometheus profile create work2 --clone-all`
- **THEN** `work2` SHALL have no Telegram adapter configuration and no pairing data

### Requirement: Profile descriptions
The system SHALL store a per-profile description (at most 500 characters) set by `prometheus profile describe <name> --text <text>` or generated by `--auto` through the auxiliary model from the profile's name, identity file, model and installed skills (refusing to overwrite an existing description without `--overwrite`; `--all` applies to every profile lacking one), shown by `profile list` and `profile show` and available to kanban assignment routing.
**Milestone:** M3

#### Scenario: Auto description does not overwrite
- **GIVEN** profile `research` with a user-written description
- **WHEN** the user runs `prometheus profile describe research --auto`
- **THEN** the description SHALL be unchanged and the command SHALL print `description exists; use --overwrite`

### Requirement: Profile distributions
The system SHALL provide `prometheus profile install <git-url|dir> [--name <n>] [--alias] [--force] [-y]` reading a `distribution.toml` manifest (`name`, `version`, `description`, `prometheus_requires` semver range, `author`, `license`, `owned` paths, `secrets_required` entries with `name`, `description`, `required`), previewing the files and required secrets and asking for confirmation, refusing an incompatible `prometheus_requires` with exit 78, prompting for missing required secrets into the secrets store, and never copying credential stores, `state.db`, sessions, memory rows, logs or snapshots even when present in the source; `profile update <name> [--force-config] [-y]` SHALL replace distribution-owned files (identity file, skills per entry, MCP definitions), merge cron jobs by id keeping the user's paused state and adding new jobs paused, and preserve the user's `config.toml` unless `--force-config`; `profile info <name>` SHALL show origin URL, commit, version and last update time, and `profile delete` SHALL show the distribution before confirming.
**Milestone:** M3

#### Scenario: Update keeps user config
- **GIVEN** an installed distribution whose user changed `model.default`
- **WHEN** the user runs `prometheus profile update research-bot -y`
- **THEN** `model.default` SHALL keep the user's value and the identity file SHALL match the new commit

#### Scenario: Credentials never installed
- **GIVEN** a distribution repository that accidentally contains `secrets.toml`
- **WHEN** it is installed
- **THEN** the new profile SHALL NOT contain that file's contents

### Requirement: Identity migration and purge
The system SHALL provide `prometheus profile identity move <from> <to>` copying a profile's identity file and identity-bearing memory rows to another profile after confirmation, and `prometheus profile identity purge <name>` removing the identity file and identity-tier memory rows after a destructive confirmation (taking a quick snapshot first), both applying from the next session.
**Milestone:** M3

#### Scenario: Purge is snapshotted
- **WHEN** the user runs `prometheus profile identity purge work --yes`
- **THEN** a snapshot with reason `manual` and label `pre-identity-purge` SHALL exist and `SOUL.md` SHALL be absent

### Requirement: External secret source commands
The system SHALL provide `prometheus secrets bitwarden setup|status|token|sync [--apply]|install|disable` and `prometheus secrets onepassword setup|status|map <NAME> <op-reference>|unmap <NAME>|sync [--apply]|disable`, where `setup` installs the pinned CLI (`bws` or `op`) into `<home>/runtimes/` after confirmation, stores the access or service-account token in the secrets store under the name recorded in `access_token_secret`, selects the project or vault (`--project-id`, `--server-url`, `--vault` for non-interactive use) and enables the source; `status` reports binary version, token validity and mapped names without values; `token` rotates the token only after validating the new one; `sync` previews which secret names would be added or shadowed and writes only with `--apply`; resolved values SHALL be cached in memory for at most `secrets.<source>.cache_ttl_secs` (default 300) and never written to disk.
**Milestone:** M3

#### Scenario: Invalid rotated token refused
- **GIVEN** an enabled Bitwarden source
- **WHEN** the user runs `prometheus secrets bitwarden token` and enters an invalid token
- **THEN** the command SHALL exit 78 and the stored token SHALL be unchanged

### Requirement: Retired model rewrite
The system SHALL provide `prometheus config fix-models [--apply] [--no-backup]` that scans every config key holding a model id (main, auxiliary, delegation, cron jobs, fallback chains) for ids marked retired or deprecated in the model catalog's retirement table, prints each finding with its replacement, changes nothing without `--apply`, and with `--apply` rewrites through the format-preserving config writer after copying `config.toml` to `config.toml.bak-<timestamp>` unless `--no-backup`; `prometheus doctor` SHALL report retired ids as `warn` with this command as remedy.
**Milestone:** M2

#### Scenario: Dry run by default
- **GIVEN** `model.default = "grok-2"` marked retired with replacement `grok-4`
- **WHEN** the user runs `prometheus config fix-models`
- **THEN** the output SHALL list `model.default: grok-2 -> grok-4` and `config.toml` SHALL be unchanged

### Requirement: Secrets are resolved per use
The system SHALL read a secret from its store at each use (with the in-memory cache bounded by the store's change notification or a 5 s staleness limit), so a secret set or rotated with `prometheus secrets set` applies to the next provider request of running sessions without a reload command, and the registry SHALL provide `/reload-secrets` that invalidates the cache immediately for the session's profile and prints the names (never values) whose value changed; environment-supplied secrets SHALL remain fixed for the process lifetime.
**Milestone:** M1

#### Scenario: Rotated key used next request
- **GIVEN** a running session using `OPENAI_API_KEY`
- **WHEN** the user runs `prometheus secrets set OPENAI_API_KEY` with a new value in another terminal and then `/reload-secrets`
- **THEN** the session's next provider request SHALL use the new value

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
The system SHALL lay out the home as in design decision D13: the root holds only `<home>/active_profile`, `<home>/daemon.toml` (daemon-wide settings: listeners, ports, relay, all-adapters-down policy), `<home>/run/` (daemon runtime records and sockets), `<home>/logs/` (daemon, update and migration logs), `<home>/runtimes/` (hash-pinned managed runtimes), `<home>/updates/` (install and update receipts), `<home>/kanban/` (boards shared across profiles) and `<home>/profiles/<name>/` for every profile including `default`; each profile directory SHALL contain `config.toml`, `secrets.toml` (when the file backend is used), `grants.toml`, `state.db`, and the directories `blobs/`, `skills/`, `harness/`, `trajectories/`, `cache/`, `scripts/`, `snapshots/`, `python/`, `memories/`, `cron/`, `logs/`, `themes/`, `plugins/`, `backups/` and `migrations/`, created on first use with mode `0700`.
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
The system SHALL refuse to store a value in a non-secret config key when it matches a known credential pattern (for example `sk-`, `ghp_`, `xoxb-`, `AKIA` prefixes or a PEM private key header), failing with `this looks like a credential; store it with: prometheus secrets set <NAME>`, and config keys that need a credential SHALL accept only a secret reference `{ secret = "<NAME>" }`.
**Milestone:** M1

#### Scenario: API key pasted into config
- **WHEN** the user runs `prometheus config set providers.acme.base_url sk-live-123456`
- **THEN** the command SHALL fail with the credential message and SHALL NOT write the value

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

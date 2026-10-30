//! The error taxonomy: every user-visible error has exactly one
//! [`ErrorKind`] and a stable dotted code from [`ERROR_CODES`]
//! (spec: workspace-architecture, Error taxonomy).
//!
//! Codes are declared once in the `error_codes!` block below, which defines
//! each code constant and its catalog entry together: a code that is missing
//! from the generated catalog cannot be constructed.

use std::fmt;

use serde::Serialize;

/// The closed set of error kinds. Each maps to exactly one process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Unknown command, flag or argument.
    Usage,
    /// Malformed input data.
    InvalidInput,
    /// A named thing does not exist.
    NotFound,
    /// Configuration is invalid or names something unregistered.
    Config,
    /// A credential is missing or rejected.
    Auth,
    /// The operation is not allowed.
    PermissionDenied,
    /// A spending or token budget is exhausted.
    BudgetExceeded,
    /// A model provider failed or is unreachable.
    Provider,
    /// A tool reported failure.
    Tool,
    /// The sandbox could not be set up or refused the operation.
    Sandbox,
    /// Another writer or holder owns the resource.
    Conflict,
    /// A dependency (daemon, runtime, surface) is unavailable.
    Unavailable,
    /// The peer speaks an incompatible protocol version.
    Protocol,
    /// The user or the system cancelled the operation.
    Cancelled,
    /// A filesystem or stream operation failed.
    Io,
    /// A bug: invariant violation or panic.
    Internal,
}

impl ErrorKind {
    /// Every kind, in catalog order.
    pub const ALL: [Self; 16] = [
        Self::Usage,
        Self::InvalidInput,
        Self::NotFound,
        Self::Config,
        Self::Auth,
        Self::PermissionDenied,
        Self::BudgetExceeded,
        Self::Provider,
        Self::Tool,
        Self::Sandbox,
        Self::Conflict,
        Self::Unavailable,
        Self::Protocol,
        Self::Cancelled,
        Self::Io,
        Self::Internal,
    ];

    /// The wire name used in `--json` output and RPC error data.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::InvalidInput => "invalid_input",
            Self::NotFound => "not_found",
            Self::Config => "config",
            Self::Auth => "auth",
            Self::PermissionDenied => "permission_denied",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Sandbox => "sandbox",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
            Self::Protocol => "protocol",
            Self::Cancelled => "cancelled",
            Self::Io => "io",
            Self::Internal => "internal",
        }
    }

    /// The process exit code a command failing with this kind returns; one
    /// of [`EXIT_CODES`](crate::exit::EXIT_CODES).
    ///
    /// Auth shares 78 with config because the table defines 78 as
    /// "configuration or credential error". A tool failure is a reported
    /// failure of a command that ran (1). A sandbox refusal is a security
    /// posture failure (77). A conflict is a held lock or writer (75,
    /// retryable).
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::InvalidInput => 65,
            Self::NotFound => 66,
            Self::Config | Self::Auth => 78,
            Self::PermissionDenied | Self::Sandbox => 77,
            Self::BudgetExceeded => 3,
            Self::Provider | Self::Unavailable => 69,
            Self::Tool => 1,
            Self::Conflict => 75,
            Self::Protocol => 76,
            Self::Cancelled => 130,
            Self::Io => 74,
            Self::Internal => 70,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the error-code catalog.
#[derive(Debug, PartialEq, Eq)]
pub struct ErrorCode {
    /// Stable dotted code, for example `config.unknown_key`.
    pub code: &'static str,
    /// The kind every error with this code carries.
    pub kind: ErrorKind,
    /// What the error means, as documented for users.
    pub summary: &'static str,
}

/// Declares error codes: one `pub const` per code in [`codes`] and the
/// matching entry in [`ERROR_CODES`].
macro_rules! error_codes {
    ($( $(#[$attr:meta])* $name:ident = ($code:literal, $kind:ident, $summary:literal); )*) => {
        /// Every registered error code, one constant per catalog entry.
        pub mod codes {
            use super::{ErrorCode, ErrorKind};
            $(
                $(#[$attr])*
                pub const $name: ErrorCode = ErrorCode {
                    code: $code,
                    kind: ErrorKind::$kind,
                    summary: $summary,
                };
            )*
        }

        /// The catalog: every code an [`Error`] can carry, in declaration order.
        pub const ERROR_CODES: &[&ErrorCode] = &[$(&codes::$name),*];
    };
}

// New codes go here; `cargo xtask gen` then regenerates the catalog page.
error_codes! {
    /// An id string does not have the expected `<prefix>_<ULID>` shape.
    ID_MALFORMED = ("id.malformed", InvalidInput,
        "An id does not have the expected `<prefix>_<ULID>` shape.");
    /// A configured name is not in its closed registry.
    REGISTRY_UNKNOWN_NAME = ("registry.unknown_name", Config,
        "A configured name (provider, backend, toolset, engine, hook...) is not registered; the error lists the registered names.");
    /// Two entries were registered under one name.
    REGISTRY_DUPLICATE_NAME = ("registry.duplicate_name", Config,
        "Two entries (for example two plugins) register the same name in one registry.");
    /// A task panicked; its crash report is under `logs/crash/`.
    TASK_PANICKED = ("task.panicked", Internal,
        "A task panicked. It was isolated and a crash report was written under `logs/crash/`.");
    /// A task stopped because it was cancelled.
    TASK_CANCELLED = ("task.cancelled", Cancelled,
        "The operation was cancelled (interrupt, `/stop` or shutdown).");
    /// The launch environment was read a second time.
    LAUNCH_ENV_REREAD = ("launch_env.reread", Internal,
        "The process environment was read a second time; it may only be read once at startup.");
    /// The command line did not parse.
    CLI_USAGE = ("cli.usage", Usage,
        "The command line names an unknown command, flag or argument, or misses a required one.");
    /// The Prometheus home could not be determined.
    HOME_UNRESOLVED = ("home.unresolved", Config,
        "No Prometheus home could be determined: `--home` and `PROMETHEUS_HOME` are unset and the user home directory is unknown.");
    /// A file under the Prometheus home could not be read or written.
    HOME_IO = ("home.io", Io,
        "A file or directory under the Prometheus home could not be read or written.");
    /// Another process holds the home's single-writer lock.
    HOME_LOCKED = ("home.locked", Conflict,
        "Another Prometheus process (the daemon or an in-process chat) holds the home's writer lock `run/gateway.lock`; retry when it exits.");
    /// A configuration file is not valid TOML or does not match its schema.
    CONFIG_PARSE = ("config.parse", Config,
        "A configuration file (`config.toml`, `daemon.toml`) is not valid TOML or does not match its schema.");
    /// A profile `config.toml` names a key the schema does not define.
    CONFIG_UNKNOWN_KEY = ("config.unknown_key", Config,
        "A profile `config.toml` contains a key the schema does not define (the error suggests the closest key), or a machine-wide section that belongs in `daemon.toml`.");
    /// A profile `config.toml` value has the wrong type, range or enum value.
    CONFIG_INVALID = ("config.invalid", Config,
        "A profile `config.toml` value has the wrong type, is out of range or is not one of the allowed values; the error names the key, line and allowed values.");
    /// A key or value given on the command line is unknown or invalid.
    CONFIG_INVALID_ARGUMENT = ("config.invalid_argument", Usage,
        "A configuration key or value given on the command line (`config set`, `config get`, `--set`) is unknown, malformed, out of range or not allowed.");
    /// A value that looks like a credential was given for a non-secret key.
    CONFIG_CREDENTIAL_VALUE = ("config.credential_value", Usage,
        "A value that looks like a credential (API key, token, private key) was given for a non-secret configuration key; store it with `prometheus secrets set <NAME>`.");
    /// A profile `config.toml` holds a credential in a non-secret key.
    CONFIG_CREDENTIAL_IN_FILE = ("config.credential_in_file", Config,
        "A profile `config.toml` holds a value that looks like a credential (API key, token, private key) in a non-secret key; move it into the secrets store with `prometheus secrets set <NAME>` and reference it by name.");
    /// `config.toml` was written by a newer prometheus.
    CONFIG_TOO_NEW = ("config.too_new", Config,
        "`config.toml` has a `schema_version` newer than this binary supports; upgrade prometheus.");
    /// The configuration editor could not be started or failed.
    CONFIG_EDITOR = ("config.editor", Unavailable,
        "The editor for `prometheus config edit` (`VISUAL`, `EDITOR`, else `vi` / `notepad`) could not be started or exited with an error.");
    /// A profile name does not match the profile name rules.
    PROFILE_INVALID_NAME = ("profile.invalid_name", Usage,
        "A profile name does not match `^[a-z0-9][a-z0-9_-]{0,63}$` after lowercasing.");
    /// The profile selected at launch does not exist in this home (a usage
    /// error, exit 2, per the profile-selection spec).
    PROFILE_NOT_FOUND = ("profile.not_found", Usage,
        "The profile named by `-p` or `active_profile` does not exist in this home; create it with `prometheus profile create <name>`.");
    /// A secret name does not match the secret name rules.
    SECRET_INVALID_NAME = ("secret.invalid_name", Usage,
        "A secret name does not match `^[A-Z][A-Z0-9_]{0,127}$`.");
    /// A secret value was given on the command line.
    SECRET_VALUE_ON_ARGV = ("secret.value_on_argv", Usage,
        "A secret value was given as a command-line argument; values are read only from a no-echo prompt or stdin, so they never appear in shell history or process listings.");
    /// The secret value read from the prompt or stdin is empty.
    SECRET_EMPTY = ("secret.empty", InvalidInput,
        "The secret value read from the prompt or stdin is empty.");
    /// A secret the profile needs is not set.
    SECRET_MISSING = ("secret.missing", Config,
        "A secret the profile needs is not in its secrets store (nor, for the launch profile, in the environment); set it with `prometheus secrets set <NAME>`.");
    /// The secrets store cannot be read or written.
    SECRET_STORE = ("secret.store", Config,
        "The profile's secrets store (`secrets.toml` or the OS keychain) cannot be read, parsed or written.");
    /// The OS keychain backend is not available on this machine.
    SECRET_KEYCHAIN_UNAVAILABLE = ("secret.keychain_unavailable", Unavailable,
        "`secrets.backend = \"keychain\"` is configured but no OS keychain is reachable (no Secret Service on Linux, locked keychain); use the `file` backend.");
    /// Revealing a secret was refused because stdout is not a terminal.
    SECRET_REVEAL_REFUSED = ("secret.reveal_refused", PermissionDenied,
        "`prometheus secrets get --reveal` prints a value only to an interactive terminal, never into a pipe or file.");
    /// A profile name is reserved.
    PROFILE_RESERVED = ("profile.reserved", Usage,
        "A new profile may not use a reserved name (`prometheus`, `pmt`, `test`, `tmp`, `root`, `all`, `none`, `run`, `logs`, `tools`).");
    /// The profile to create already exists.
    PROFILE_EXISTS = ("profile.exists", Usage,
        "`prometheus profile create` or `rename` names a profile that already exists.");
    /// The operation is not allowed on the default profile.
    PROFILE_PROTECTED = ("profile.protected", Usage,
        "The `default` profile cannot be deleted or renamed.");
    /// A running process uses the profile.
    PROFILE_IN_USE = ("profile.in_use", Conflict,
        "A running daemon or interactive client uses the profile; stop it and retry.");
    /// The profile was deleted.
    PROFILE_DELETED = ("profile.deleted", NotFound,
        "The profile was deleted (`profiles/.deleted/<name>` exists); nothing may write to it until it is created again.");
    /// Another backup of this home is running.
    BACKUP_RUNNING = ("backup.running", Conflict,
        "Another `prometheus backup create` is running for this home.");
    /// A backup archive is corrupt or does not match its manifest.
    BACKUP_CORRUPT = ("backup.corrupt", InvalidInput,
        "A backup archive or snapshot is unreadable, misses its manifest, or a member does not match its manifest size and SHA-256 hash; nothing was restored.");
    /// A backup archive or snapshot could not be written.
    BACKUP_IO = ("backup.io", Io,
        "A backup archive or snapshot could not be written or read.");
    /// No snapshot has the given id.
    SNAPSHOT_NOT_FOUND = ("snapshot.not_found", NotFound,
        "No snapshot of this profile has the given id; list them with `prometheus snapshots list`.");
    /// Writing a command's output or reading an answer failed.
    OUTPUT_IO = ("output.io", Io,
        "Writing the command's output to stdout (for example into a closed pipe) or reading an answer from stdin failed.");
    /// The invoked surface was not compiled into this build.
    SURFACE_NOT_INCLUDED = ("surface.not_included", Unavailable,
        "The invoked surface (tui, gateway, dashboard, acp, mcp, evolve, eval) is not included in this build.");
    /// The profile's identity file is unreadable or malformed.
    PROFILE_ID_INVALID = ("profile.id_invalid", Config,
        "The profile's `profile_id` file is unreadable or does not hold a `prof_<ULID>` id; restore it from a backup.");
    /// `state.db` belongs to another profile.
    STORE_PROFILE_MISMATCH = ("store.profile_mismatch", Config,
        "The profile's `state.db` records another profile's id (it was copied from another profile); nothing was written. Restore the profile's own database or move the file away.");
    /// The store was written by a newer build.
    STORE_SCHEMA_TOO_NEW = ("store.schema_too_new", Config,
        "`state.db` has a newer schema than this build supports: it is read-only (or unreadable when the newer build says so). Update Prometheus.");
    /// A typed column got a value outside its enum.
    STORE_INVALID_ENUM = ("store.invalid_enum", InvalidInput,
        "A session field (source, end reason, archive state, title source, lineage kind, role) got a value outside its allowed set.");
    /// A write violated a store constraint.
    STORE_CONSTRAINT = ("store.constraint", InvalidInput,
        "A write violated a store constraint (duplicate id, unknown session); the whole transaction was rolled back.");
    /// `state.db` or a blob could not be read or written.
    STORE_IO = ("store.io", Io,
        "`state.db` or a blob file could not be read or written (disk full, permissions, I/O error).");
    /// SQLite reported an unexpected error.
    STORE_SQLITE = ("store.sqlite", Internal,
        "SQLite reported an unexpected error on `state.db`.");
    /// WAL is not safe on the store's filesystem.
    STORE_UNSUPPORTED_FILESYSTEM = ("store.unsupported_filesystem", Config,
        "`state.db` is on a network or cross-VM filesystem where WAL is unsafe; set `storage.journal_mode = \"delete\"` or move the home to a local disk.");
    /// A blob's bytes do not match its SHA-256 name.
    STORE_BLOB_CORRUPT = ("store.blob_corrupt", Io,
        "A blob file's contents do not match its SHA-256 name.");
    /// A referenced blob file is missing.
    STORE_BLOB_MISSING = ("store.blob_missing", NotFound,
        "A row references a blob that is not in the profile's blob store.");
    /// A message the compactor read changed before the commit.
    STORE_COMPACTION_CONFLICT = ("store.compaction_conflict", Conflict,
        "A message the compaction read changed (rewound, compacted or rewritten) before it committed; nothing changed. Retry the compaction.");
    /// The caller's turn lease or compaction lock is no longer held.
    STORE_LEASE_LOST = ("store.lease_lost", Conflict,
        "The turn lease or compaction lock expired and was taken by another holder; nothing changed.");
    /// A title is longer than 100 characters.
    STORE_TITLE_TOO_LONG = ("store.title_too_long", InvalidInput,
        "Session titles are at most 100 characters.");
    /// Another session of the profile has the title.
    STORE_TITLE_IN_USE = ("store.title_in_use", InvalidInput,
        "Session titles are unique per profile; pick another title.");
    /// No session matches a reference.
    SESSION_NOT_FOUND = ("session.not_found", NotFound,
        "No session of the profile matches the id, id prefix or title; list them with `prometheus sessions list`.");
    /// A session reference matches several sessions.
    SESSION_AMBIGUOUS = ("session.ambiguous", Usage,
        "A session id prefix matches several sessions; give more characters or the full id.");
    /// Another writer held the store past the write's time budget.
    STORE_BUSY = ("store.busy", Conflict,
        "Another process held `state.db`'s write lock for longer than the write's time budget (20 s metadata, 60 s transcript, 0.5 s activity); retry later.");
    /// `state.db` was replaced underneath an open store.
    STORE_REPLACED = ("store.replaced", Conflict,
        "`state.db` was replaced (restored or copied over) while it was open; the write was refused and the file left untouched. Restart the process to open the new file.");
    /// The store hit structural corruption and stopped writing.
    STORE_CORRUPT = ("store.corrupt", Io,
        "`state.db` reported structural corruption; writes stopped. The next start quarantines the file and salvages every readable row (see `prometheus doctor`).");
    /// A rewind target is missing or not a user message.
    STORE_REWIND_TARGET_INVALID = ("store.rewind_target_invalid", InvalidInput,
        "Rewind targets an active user message of the session; the given message is missing, inactive or not a user message.");
    /// The transcript changed since the rewind or restore was planned.
    STORE_REWIND_CONFLICT = ("store.rewind_conflict", Conflict,
        "The messages the rewind or restore expected changed in the meantime; nothing changed. Look at the transcript again and retry.");
    /// A session is used by a running turn or compaction.
    SESSION_IN_USE = ("session.in_use", Conflict,
        "A live turn lease or compaction lock is held on the session; retry when the turn or compaction finished.");
    /// A lineage has too many active messages to resume.
    SESSION_TOO_LARGE_TO_RESUME = ("session.too_large_to_resume", InvalidInput,
        "The session's active lineage has more messages than `sessions.max_resume_messages`; compact it with /compress or fork it.");
    /// A session has too many messages for an in-memory export.
    SESSION_TOO_LARGE_TO_EXPORT = ("session.too_large_to_export", InvalidInput,
        "The session has more messages than `sessions.max_export_messages` allows in memory; export it to a file with `prometheus sessions export -o FILE`.");
    /// A session source tag does not have the allowed shape.
    SESSION_INVALID_SOURCE_TAG = ("session.invalid_source_tag", Usage,
        "A session source tag is 1-32 characters: a lowercase letter, then lowercase letters, digits, '_' or '-'.");
    /// A trajectory was written by a newer grammar major version.
    TRAJECTORY_GRAMMAR_TOO_NEW = ("trajectory.grammar_too_new", Protocol,
        "The trajectory was written with a newer major grammar version than this build reads; run `prometheus update`.");
    /// A trajectory line is not a valid event.
    TRAJECTORY_MALFORMED = ("trajectory.malformed", InvalidInput,
        "A trajectory line is not valid JSON or lacks the grammar's top-level keys; the message names the line.");
    /// A session has no trajectory file.
    TRAJECTORY_NOT_FOUND = ("trajectory.not_found", NotFound,
        "The session has no trajectory: it was not recorded (`observability.trajectories.enabled = false`) or retention removed it.");
    /// A trajectory file could not be read or written.
    TRAJECTORY_IO = ("trajectory.io", Io,
        "A trajectory file under `<profile>/trajectories/` could not be read or written.");
    /// A session's trajectory reached its size limit.
    TRAJECTORY_SIZE_LIMIT = ("trajectory.size_limit", Io,
        "The session's trajectory reached `observability.trajectories.max_bytes_per_session`; later events are recorded without their payload. Recorded as an `error` event, not returned.");
    /// A `prometheus logs --since` value does not parse.
    LOGS_INVALID_SINCE = ("logs.invalid_since", Usage,
        "`prometheus logs --since` takes `<N>[smhd]` (30m, 2h, 1d) or an RFC 3339 time.");
    /// A log file could not be read.
    LOGS_IO = ("logs.io", Io,
        "A log file under `logs/` could not be read.");
    /// A `prometheus usage --since/--until` value does not parse.
    USAGE_INVALID_TIME = ("usage.invalid_time", Usage,
        "`prometheus usage --since/--until` take `<N>[smhd]` (30m, 2h, 7d) or an RFC 3339 time.");
    /// A model call would exceed a configured budget.
    BUDGET_EXCEEDED = ("budget.exceeded", BudgetExceeded,
        "A model call was not sent because its worst-case cost or tokens would exceed a `budgets.*` limit; raise it with `prometheus config set budgets.<key>` or `/budget set`.");
    /// A USD budget applies but the model has no known price.
    BUDGET_UNPRICED_MODEL = ("budget.unpriced_model", BudgetExceeded,
        "A USD budget is set but the model has no price; set `models.<model>.price` or `budgets.allow_unpriced = true` (then only token budgets apply to it).");
    /// An agent tool tried to change a budget.
    BUDGET_AGENT_WRITE = ("budget.agent_write", PermissionDenied,
        "Budgets are changed only by the user (`prometheus config set`, `/budget set`); agent tools may not write `budgets.*` keys, including through file writes to `config.toml`.");
    /// The configured profile time zone is unknown.
    CONFIG_TIMEZONE = ("config.timezone", Config,
        "`cron.timezone` must be an IANA time zone name such as `Europe/Lisbon`.");
    /// Telemetry was refused for an unsafe endpoint.
    TELEMETRY_REFUSED = ("telemetry.refused", Config,
        "`telemetry.enabled = true` needs an `https://` `telemetry.endpoint` (plain HTTP only to loopback); nothing is sent otherwise. Logged, not returned.");
    /// The shared outbound HTTP client could not be built.
    NET_CLIENT_INIT = ("net.client_init", Internal,
        "The shared outbound HTTP client could not be initialised.");
    /// A fetch targeted a private, metadata or unresolvable destination.
    NET_DESTINATION_BLOCKED = ("net.destination_blocked", PermissionDenied,
        "A URL resolved to a private or cloud-metadata address, or did not resolve; private destinations need `security.egress.allow_private` or a session grant, metadata endpoints are always refused.");
    /// A fetch followed more redirects than allowed.
    NET_TOO_MANY_REDIRECTS = ("net.too_many_redirects", Unavailable,
        "A fetched URL redirected more than 10 times.");
    /// A fetched URL is malformed or uses a scheme other than http(s).
    NET_INVALID_URL = ("net.invalid_url", InvalidInput,
        "A URL to fetch is malformed or is not `http://` / `https://`.");
    /// An HTTP request failed in transport (connect, TLS, read).
    NET_REQUEST = ("net.request", Unavailable,
        "An outbound HTTP request failed: connection, TLS verification or transfer error.");
    /// `network.extra_ca_file` could not be read or holds no certificate.
    NET_EXTRA_CA = ("net.extra_ca", Config,
        "`network.extra_ca_file` cannot be read or contains no PEM certificate.");
    /// A listener was asked to bind a non-loopback address without an explicit, authenticated configuration.
    NET_LISTEN_REFUSED = ("net.listen_refused", PermissionDenied,
        "A network surface may bind a non-loopback address only when its configuration sets that address explicitly and an allowlist or auth provider is configured.");
    /// A listener could not bind its address.
    NET_LISTEN = ("net.listen", Io,
        "A listener could not bind its address (in use or not permitted).");
    /// A protected-instruction-file pattern is invalid.
    SECURITY_INVALID_PATTERN = ("security.invalid_pattern", Config,
        "An entry of `security.protected_instruction_extra_patterns` is not a valid basename glob.");
    /// A capability string is not in the closed vocabulary.
    CAPABILITY_UNKNOWN = ("capability.unknown", Config,
        "A capability string (config, plugin manifest, grants file) is not in the closed vocabulary (`fs.read:<glob>`, `fs.write:<glob>`, `net.connect:<host>[:<port>]`, `process.spawn:<program>`...).");
    /// A capability was used or requested beyond what was granted.
    CAPABILITY_NOT_GRANTED = ("capability.not_granted", PermissionDenied,
        "A capability was used or requested that the session, parent, plugin approval or grant does not include (`capability not granted: <capability>`); grant it when prompted, or review grants with `prometheus grants list`.");
    /// `grants.toml` cannot be read, parsed or written.
    GRANTS_STORE = ("grants.store", Config,
        "A profile's `grants.toml` cannot be read, is not valid (bad TOML, unknown capability or subject) or cannot be written; fix or remove the offending entry.");
    /// No grant has the given id.
    GRANTS_NOT_FOUND = ("grants.not_found", NotFound,
        "No grant in the profile's `grants.toml` has this id; list them with `prometheus grants list`.");
    /// A grant subject does not have a recognized form.
    GRANTS_INVALID_SUBJECT = ("grants.invalid_subject", Usage,
        "A grant subject is not `plugin:<id>@<digest>`, `mcp:<server>`, `skill:<name>` or `profile`.");
    /// A capability prompt was denied, dismissed, timed out or had no human to ask.
    GRANTS_DENIED = ("grants.denied", PermissionDenied,
        "A capability request was denied: the human answered deny, dismissed the prompt, the surface detached, `approvals.timeout_secs` passed, or the context is unattended (no prompt is possible).");
    /// The OS sandbox lacks a hardening feature that was not explicitly allowed to be missing.
    SANDBOX_UNAVAILABLE = ("sandbox.unavailable", Sandbox,
        "The OS sandbox cannot enforce a feature (Landlock, seccomp, Seatbelt, AppContainer) on this host and `security.sandbox.allow_degraded` does not name it, so the command was not run.");
    /// The OS sandbox could not be set up for a child process.
    SANDBOX_SETUP = ("sandbox.setup", Sandbox,
        "The OS sandbox profile for a child process could not be built or applied; the child was not started.");
    /// `terminal.backend` names no registered backend.
    TERMINAL_UNKNOWN_BACKEND = ("terminal.unknown_backend", Config,
        "`terminal.backend` names a backend that is neither built in nor provided by an enabled plugin; the error lists the available backends.");
    /// The selected terminal backend ships in a later milestone.
    TERMINAL_BACKEND_UNAVAILABLE = ("terminal.backend_unavailable", Unavailable,
        "The selected terminal backend (`container`, `ssh`) is reserved but not available in this build.");
    /// `terminal.env_passthrough` (or `terminal.container.env`) names a secret.
    TERMINAL_ENV_PASSTHROUGH_SECRET = ("terminal.env_passthrough_secret", Config,
        "`terminal.env_passthrough` or `terminal.container.env` names a secret (a stored secret, a provider credential, a `PMT_SECRET_*` or secret-looking name); grant it with `secrets.read:<NAME>` instead.");
    /// The shell the local terminal backend needs is not installed.
    TERMINAL_SHELL_NOT_FOUND = ("terminal.shell_not_found", Unavailable,
        "The shell selected by `terminal.windows_shell` (Git Bash, pwsh, Windows PowerShell) is not installed, so the terminal tool is unavailable.");
    /// A provider id or alias is not in the profile's provider registry.
    PROVIDER_UNKNOWN = ("provider.unknown", Config,
        "A requested or configured provider id is neither a canonical id nor an alias in this profile's registry (built-in providers, `[providers.*]` blocks and the profile's enabled provider plugins); the message suggests the closest id.");
    /// The provider is disabled in config.
    PROVIDER_DISABLED = ("provider.disabled", Config,
        "The requested or fallback provider has `providers.<id>.enabled = false`; nothing else was resolved.");
    /// No provider could be selected automatically.
    PROVIDER_NOT_CONFIGURED = ("provider.not_configured", Config,
        "No provider is configured, no provider secret is stored, no sign-in exists and no configured local endpoint answers. Run `prometheus model` or `prometheus auth add <provider>`.");
    /// A local-server provider has no endpoint.
    PROVIDER_MISSING_BASE_URL = ("provider.missing_base_url", Config,
        "A provider that needs an endpoint (local servers, `custom`) has no `providers.<id>.base_url` and its default loopback endpoint, if any, does not answer; the request is never sent to another provider.");
    /// No credential may be sent to the provider's host.
    PROVIDER_MISSING_CREDENTIAL = ("provider.missing_credential", Auth,
        "The provider needs a credential but none is stored whose bound hosts (`key_hosts`) include the request's host; store one with `prometheus secrets set <NAME>`.");
    /// A provider descriptor is malformed.
    PROVIDER_INVALID_DESCRIPTOR = ("provider.invalid_descriptor", Config,
        "A provider descriptor (a `[providers.<id>]` block or a plugin's) misses a required field or holds an invalid value.");
    /// The provider rejected the credential or the account's plan.
    PROVIDER_AUTH_FAILED = ("provider.auth_failed", Auth,
        "The provider rejected the credential (`auth`) or the account's plan is not entitled to the API (`entitlement`).");
    /// The provider rejected the request itself.
    PROVIDER_INVALID_REQUEST = ("provider.invalid_request", InvalidInput,
        "The provider rejected the request as malformed or misconfigured (`invalid_request`, `config`); it is never retried or rerouted.");
    /// A provider call failed.
    PROVIDER_CALL_FAILED = ("provider.call_failed", Provider,
        "A model call failed after same-route retries and fallback (`billing`, `rate_limit`, `overloaded`, `server_error`, `timeout`, `network`, `context_overflow` or `content_filter`); the message names the class.");
    /// No model is selected for the resolved provider.
    MODEL_NOT_SELECTED = ("model.not_selected", Config,
        "No model id was given (`--model`, `model.default`) and the provider has no default model.");
    /// The model lacks a capability the request needs.
    MODEL_CAPABILITY_MISSING = ("model.capability_missing", Config,
        "The route's model lacks a capability the request needs (tool calling, image input, or a context window large enough for the request).");
    /// The model's context window is below `model.min_context_window`.
    MODEL_CONTEXT_TOO_SMALL = ("model.context_too_small", Config,
        "The resolved model's context window is below `model.min_context_window`; if the window is larger, set `[models.\"<provider>/<model>\"] context_window`.");
    /// The provider's wire format is implemented by a plugin that is not installed.
    PROVIDER_PLUGIN_UNAVAILABLE = ("provider.plugin_unavailable", Config,
        "The provider speaks a wire format that only a provider plugin implements (`gemini_native`, `bedrock_converse`, bridged plugins such as `github-copilot`) or is a virtual provider of a capability this build does not include (`moa`); the message names what is missing.");
    /// A `/model`, `model set` or `fallback` argument is not a model reference.
    MODEL_INVALID_REF = ("model.invalid_ref", Usage,
        "A model argument is neither a `[model.aliases]` name nor `<provider>/<model>` (or a model id of the current provider).");
    /// The interactive model picker needs a terminal.
    MODEL_REQUIRES_TERMINAL = ("model.requires_terminal", Usage,
        "`prometheus model` (the picker) or `prometheus fallback add` without an argument ran without a terminal on stdin and stdout; use `prometheus model set <provider>/<model>` instead.");
    /// The fallback chain already holds the entry.
    FALLBACK_DUPLICATE = ("fallback.duplicate", Conflict,
        "`prometheus fallback add` named a route (canonical provider, model, base URL) the chain already holds; the chain is unchanged.");
    /// No fallback entry matches.
    FALLBACK_NOT_FOUND = ("fallback.not_found", NotFound,
        "`prometheus fallback remove` named an index or `<provider>/<model>` that is not in the chain.");
    /// A reasoning setting names an unknown level or tier.
    REASONING_INVALID = ("reasoning.invalid", Config,
        "An `[agent.reasoning_overrides]` value or `[agent.reasoning_tiers]` target is neither a reasoning level (none, minimal, low, medium, high, xhigh, max, ultra) nor a tier name of `[agent.reasoning_tiers]`.");
    /// A local inference server answered unexpectedly.
    LOCAL_SERVER_PROBE = ("local.server_probe", Unavailable,
        "A local inference server (LM Studio, Ollama, llama.cpp, vLLM, SGLang) did not serve a model listing at the given root or at `<root>/v1`.");
    /// An API key was given as a command-line argument.
    AUTH_KEY_ON_ARGV = ("auth.key_on_argv", Usage,
        "`prometheus auth add` received an extra argument; API keys are read only from the hidden prompt or stdin, so they never appear in shell history or process listings.");
    /// The provider does not take the requested credential type.
    AUTH_UNSUPPORTED = ("auth.unsupported", Usage,
        "`prometheus auth add` asked for a credential type the provider does not take (an API key for a sign-in-only provider, a sign-in for a provider without sign-in flows, or a flow it does not offer).");
    /// A credential label is malformed.
    AUTH_INVALID_LABEL = ("auth.invalid_label", Usage,
        "A credential label must be 1-32 characters a-z, 0-9 or '-', starting with a letter or digit.");
    /// No credential matches the label or index.
    AUTH_NOT_FOUND = ("auth.not_found", NotFound,
        "`prometheus auth remove` named a label or 1-based index that `prometheus auth list` does not show for the provider.");
    /// The provider's sign-in is not available in this build.
    AUTH_SIGNIN_UNAVAILABLE = ("auth.signin_unavailable", Unavailable,
        "The provider declares a sign-in, but this build ships no endpoints for it and `[providers.<id>.oauth]` does not configure them.");
    /// A sign-in endpoint is not HTTPS (or plain HTTP on loopback).
    AUTH_INSECURE_ENDPOINT = ("auth.insecure_endpoint", Config,
        "A sign-in authorize, device or token endpoint is not an `https://` URL; plain `http://` is accepted only for an identity provider on a loopback address.");
    /// A sign-in endpoint host is not one the provider allows.
    AUTH_HOST_NOT_ALLOWED = ("auth.host_not_allowed", Config,
        "A sign-in endpoint's host is not in the provider's allowed sign-in hosts (`[providers.<id>.oauth] allowed_hosts`); nothing was sent to it.");
    /// The device code expired before the user approved it.
    AUTH_LOGIN_EXPIRED = ("auth.login_expired", Auth,
        "The device-code sign-in expired before it was approved in the browser; run `prometheus auth add <provider>` again.");
    /// The sign-in did not complete.
    AUTH_LOGIN_FAILED = ("auth.login_failed", Unavailable,
        "The identity provider refused or did not answer the sign-in (denied consent, an error response, repeated network failures or no callback before the timeout); nothing was stored.");
    /// The fixed loopback redirect port of a browser sign-in is in use.
    AUTH_PORT_BUSY = ("auth.port_busy", Conflict,
        "A browser sign-in needs a fixed loopback redirect port that another process is using, and the provider offers no device-code flow to fall back to.");
    /// The subscription's plan may not be used this way.
    AUTH_PLAN_NOT_ELIGIBLE = ("auth.plan_not_eligible", Auth,
        "The provider refused a request made with a valid subscription sign-in because the plan is not eligible for this use (for example Claude Pro, or Claude Max without extra-usage credits); the sign-in stays valid. Use an API key: `prometheus auth add <provider> --type api_key`.");
    /// An external sign-in file lacks a token.
    AUTH_IMPORT_INCOMPLETE = ("auth.import_incomplete", Tool,
        "The external sign-in file (`prometheus auth import`) is missing, unreadable or does not hold both an access token and a refresh token; nothing was imported.");
    /// A sign-in was revoked or expired for good.
    AUTH_REAUTH_REQUIRED = ("auth.reauth_required", Auth,
        "The provider refused the sign-in's refresh token for good (revoked, expired or reused), so the sign-in is quarantined as `dead` and its refresh token is never sent again; run `prometheus auth add <provider>` to sign in again.");
    /// Every credential of the provider is benched.
    AUTH_CREDENTIALS_BENCHED = ("auth.credentials_benched", Unavailable,
        "Every credential of the provider is benched after a rate-limit, billing or authentication failure; requests resume when the earliest bench ends (shown by `prometheus auth status <provider>`).");
    /// A token refresh did not complete.
    AUTH_REFRESH_FAILED = ("auth.refresh_failed", Unavailable,
        "Refreshing a sign-in's access token failed for a transient reason (network failure, a server error, or a borrowed login that could not be written back); the sign-in is unchanged and the next request tries again.");
}

/// A user-visible error: a catalog code (which fixes the kind), a one-line
/// message and an optional hint naming the command that fixes it.
#[derive(Debug)]
pub struct Error {
    code: &'static ErrorCode,
    message: String,
    hint: Option<String>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    /// An error with a catalog code and a one-line message.
    #[must_use]
    pub fn new(code: &'static ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            source: None,
        }
    }

    /// Adds a hint, usually the command that fixes the problem.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Attaches the underlying cause (kept for logs, never shown as the message).
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The error kind, fixed by the code.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.code.kind
    }

    /// The catalog entry.
    #[must_use]
    pub fn code(&self) -> &'static ErrorCode {
        self.code
    }

    /// The one-line message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The hint, if any.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The process exit code for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.kind().exit_code()
    }

    /// The `--json` failure document `{"error": {kind, code, message, hint}}`
    /// that a failing command prints to stderr.
    ///
    /// # Panics
    ///
    /// Never: the document holds only strings.
    #[must_use]
    pub fn to_json(&self) -> String {
        #[derive(Serialize)]
        struct Body<'a> {
            kind: ErrorKind,
            code: &'static str,
            message: &'a str,
            hint: Option<&'a str>,
        }
        #[derive(Serialize)]
        struct Envelope<'a> {
            error: Body<'a>,
        }
        let doc = Envelope {
            error: Body {
                kind: self.kind(),
                code: self.code.code,
                message: &self.message,
                hint: self.hint(),
            },
        };
        serde_json::to_string(&doc).expect("plain strings always serialize")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// Shorthand for results whose error is a user-visible [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{ERROR_CODES, Error, ErrorKind, codes};
    use crate::exit::EXIT_CODES;

    #[test]
    fn every_kind_maps_to_exactly_one_documented_exit_code() {
        for kind in ErrorKind::ALL {
            let documented = EXIT_CODES
                .iter()
                .filter(|e| e.code == kind.exit_code())
                .count();
            assert_eq!(documented, 1, "{kind} -> {}", kind.exit_code());
        }
    }

    #[test]
    fn kind_table_is_the_spec_table() {
        let expected = [
            ("usage", 2),
            ("invalid_input", 65),
            ("not_found", 66),
            ("config", 78),
            ("auth", 78),
            ("permission_denied", 77),
            ("budget_exceeded", 3),
            ("provider", 69),
            ("tool", 1),
            ("sandbox", 77),
            ("conflict", 75),
            ("unavailable", 69),
            ("protocol", 76),
            ("cancelled", 130),
            ("io", 74),
            ("internal", 70),
        ];
        let actual: Vec<_> = ErrorKind::ALL
            .iter()
            .map(|k| (k.as_str(), k.exit_code()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn serialized_kind_is_the_wire_name() {
        for kind in ErrorKind::ALL {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, kind.as_str());
        }
    }

    #[test]
    fn catalog_codes_are_unique_and_dotted() {
        let mut seen = HashSet::new();
        for entry in ERROR_CODES {
            assert!(seen.insert(entry.code), "duplicate code {}", entry.code);
            let parts: Vec<_> = entry.code.split('.').collect();
            let well_formed = parts.len() >= 2
                && parts.iter().all(|p| {
                    !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                });
            assert!(well_formed, "code {} is not dotted snake_case", entry.code);
        }
    }

    #[test]
    fn json_document_carries_kind_code_message_and_hint() {
        let err = Error::new(&codes::REGISTRY_UNKNOWN_NAME, "unknown provider 'x'")
            .with_hint("available providers: a, b");
        let doc: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({"error": {
                "kind": "config",
                "code": "registry.unknown_name",
                "message": "unknown provider 'x'",
                "hint": "available providers: a, b",
            }})
        );
        assert_eq!(err.exit_code(), 78);
    }

    #[test]
    fn json_document_has_null_hint_when_absent() {
        let err = Error::new(&codes::TASK_PANICKED, "task 'x' panicked");
        let doc: serde_json::Value = serde_json::from_str(&err.to_json()).unwrap();
        assert_eq!(doc["error"]["hint"], serde_json::Value::Null);
        assert_eq!(doc["error"]["kind"], "internal");
    }
}

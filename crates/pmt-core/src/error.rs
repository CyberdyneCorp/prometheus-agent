//! The error taxonomy: every user-visible error has exactly one
//! [`ErrorKind`] and a stable dotted code from [`ERROR_CODES`]
//! (spec: workspace-architecture, Error taxonomy).
//!
//! Codes are declared once in the `error_codes!` block below, which defines
//! each code constant and its catalog entry together: a code that is missing
//! from the generated catalog cannot be constructed.

use std::fmt;

use serde::Serialize;

use crate::exit;

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
            Self::Usage => exit::USAGE,
            Self::InvalidInput => exit::INVALID_INPUT,
            Self::NotFound => exit::NOT_FOUND,
            Self::Config | Self::Auth => exit::CONFIG,
            Self::PermissionDenied | Self::Sandbox => exit::PERMISSION_DENIED,
            Self::BudgetExceeded => exit::BUDGET_EXHAUSTED,
            Self::Provider | Self::Unavailable => exit::UNAVAILABLE,
            Self::Tool => exit::FAILURE,
            Self::Conflict => exit::TEMPORARY_FAILURE,
            Self::Protocol => exit::PROTOCOL_MISMATCH,
            Self::Cancelled => exit::INTERRUPTED,
            Self::Io => exit::IO,
            Self::Internal => exit::INTERNAL,
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
    /// A writer tried to change a key the administrator locked.
    CONFIG_MANAGED_KEY = ("config.managed_key", PermissionDenied,
        "The key is listed in `locked` of the administrator-managed `managed.toml`; no writer (`config set|unset|edit`, `--set`, setup) may change it.");
    /// The administrator-managed configuration layer is invalid.
    CONFIG_MANAGED_INVALID = ("config.managed_invalid", Config,
        "`managed.toml` (or `managed-secrets.toml`) beside it does not validate, or its permissions are not 0600/0640 owned by root; every command fails closed until the administrator fixes it.");
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
    /// `prometheus secrets` tried to change an administrator-managed secret.
    SECRET_MANAGED = ("secret.managed", PermissionDenied,
        "The secret comes from the administrator-managed `managed-secrets.toml`, which takes precedence over every profile store and the environment; it cannot be set or unset per profile.");
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
    /// An alias launcher would shadow something or has a bad name.
    PROFILE_ALIAS_REFUSED = ("profile.alias_refused", Usage,
        "`profile alias` refuses a launcher name that is reserved, is a `prometheus` subcommand, would shadow an existing executable on `PATH`, or is not a valid profile-style name.");
    /// A profile archive is not one `profile export` wrote, or is damaged.
    PROFILE_ARCHIVE_INVALID = ("profile.archive_invalid", InvalidInput,
        "The file is not a profile archive written by `prometheus profile export`, a member is outside the allowed set, or it is damaged; nothing was imported.");
    /// The passphrase does not open an archive's secrets.
    PROFILE_ARCHIVE_PASSPHRASE = ("profile.archive_passphrase", Auth,
        "The archive's secrets are encrypted with another passphrase (Argon2id key, XChaCha20-Poly1305); nothing was imported.");
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
        "The invoked surface (tui, gateway, dashboard, acp, mcp, evolve, eval) is not included in this build, or `plugins test-adapter <platform>` ran in a build without the first-party adapter kits (`adapter-kits` feature).");
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
    /// The fork point is not an active message of the session.
    STORE_FORK_TARGET_INVALID = ("store.fork_target_invalid", InvalidInput,
        "`session.fork` names an `at_message_id` that is not an active message of the forked session.");
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
    /// A turn was started without a profile scope.
    TURN_SCOPE_REQUIRED = ("turn.scope_required", Internal,
        "Every turn runs inside an explicit profile scope; a surface that has none bound is a bug (the default profile is never assumed).");
    /// A turn was started with another profile's scope.
    TURN_SCOPE_MISMATCH = ("turn.scope_mismatch", Internal,
        "The profile scope passed to a turn is not the profile the session's agent was built for.");
    /// Another process kept the session's turn lease past the admission wait.
    TURN_LEASE_UNAVAILABLE = ("turn.lease_unavailable", Conflict,
        "Another process is running a turn on this session; retry when it finished.");
    /// The turn was interrupted by the user or the system.
    TURN_INTERRUPTED = ("turn.interrupted", Cancelled,
        "The turn was stopped before it produced a final response (user interrupt, lost turn lease, shutdown).");
    /// The model answered with neither text nor tool calls.
    TURN_EMPTY_RESPONSE = ("turn.empty_response", Provider,
        "The model returned an empty response; retry, or switch model with `/model`.");
    /// The conversation no longer fits the model's context window.
    TURN_CONTEXT_EXHAUSTED = ("turn.context_exhausted", Provider,
        "The request exceeds the model's context window and could not be compacted; start a new session or use a model with a larger window.");
    /// A tool loop guardrail stopped the turn.
    TURN_GUARDRAIL_STOP = ("turn.guardrail_stop", Cancelled,
        "The model kept repeating a failing or unproductive tool call; see `tool_loop_guardrails` to tune the thresholds.");
    /// A steer message without text.
    TURN_EMPTY_STEER = ("turn.empty_steer", Usage,
        "A steer message needs text; empty or whitespace-only steers are rejected.");
    /// An unknown personality name.
    PERSONALITY_UNKNOWN = ("personality.unknown", Usage,
        "`agent.personality` and `/personality` take `none`, a built-in personality or a key of `[agent.personalities]`.");
    /// `prometheus prompt size --platform` named an unknown surface.
    PROMPT_UNKNOWN_PLATFORM = ("prompt.unknown_platform", Usage,
        "`--platform` takes a surface with a built-in platform hint (cli, tui, desktop, telegram, discord, ...).");
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
    /// The selected shell cannot run inside the OS sandbox.
    TERMINAL_SHELL_CONFINED = ("terminal.shell_confined", Unavailable,
        "`terminal.windows_shell = \"git-bash\"` while the Windows sandbox confines commands: the MSYS2 runtime behind Git Bash cannot start inside an AppContainer, so the terminal tool is unavailable; select `auto`, `pwsh` or `powershell`.");
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
    /// No model, provider or launch endpoint is configured.
    MODEL_NOT_CONFIGURED = ("model.not_configured", Config,
        "The profile names no model and no provider, and no `--model`, `--provider` or `--base-url` was given, so a chat has nothing to talk to. Run `prometheus setup model` (or `prometheus model`).");
    /// `-c`/`--resume latest` found no session.
    CHAT_NOTHING_TO_CONTINUE = ("chat.nothing_to_continue", NotFound,
        "`-c`, `-c <name>` or `--resume latest` found no CLI session in the directory (or none in the named lineage); start a new chat instead.");
    /// `--resume @claude-code|@codex` needs the agent-log importer.
    CHAT_IMPORT_UNAVAILABLE = ("chat.import_unavailable", Unavailable,
        "`--resume @claude-code` and `--resume @codex` import that agent's most recent log through the migration importer, which arrives with M2; resume a Prometheus session instead.");
    /// A one-shot run needed an approval nobody can give.
    CHAT_APPROVAL_REQUIRED = ("chat.approval_required", PermissionDenied,
        "A tool call of a one-shot run (`-z`, piped) needed an approval and `approvals.single_query_mode` denies it; the run ended with exit 4. Run interactively, or pass `--approvals off` / set `approvals.single_query_mode = \"approve\"`.");
    /// `--cwd` names no directory.
    CHAT_CWD_MISSING = ("chat.cwd_missing", NotFound,
        "The `--cwd` directory does not exist or is not a directory.");
    /// `--prompt-file` names no readable file.
    CHAT_PROMPT_FILE = ("chat.prompt_file", NotFound,
        "The `--prompt-file` file does not exist or cannot be read.");
    /// A one-shot run read no prompt.
    CHAT_EMPTY_PROMPT = ("chat.empty_prompt", Usage,
        "A one-shot run (`-z`, or stdin not a terminal) got an empty prompt from the flag, `--prompt-file` and stdin.");
    /// An image attachment cannot be used.
    CHAT_IMAGE = ("chat.image", InvalidInput,
        "An `--image` or `/image` file is missing, unreadable, larger than 20 MiB or not a PNG, JPEG, GIF or WebP image.");
    /// The session's model cannot take an attached image.
    CHAT_IMAGE_UNSUPPORTED = ("chat.image_unsupported", Config,
        "The resolved model's metadata has `supports_vision = false` and no `[auxiliary.vision]` route is configured to describe the image; switch to a vision model or configure `auxiliary.vision.provider`/`model`. Nothing was sent to the provider.");
    /// `--usage-file` cannot be written.
    CHAT_USAGE_FILE = ("chat.usage_file", Io,
        "The `--usage-file` path cannot be created or written (missing directory, permissions); the run itself finished and the command exits 73.");
    /// A required setup value is missing in a non-interactive run.
    SETUP_MISSING_VALUE = ("setup.missing_value", Config,
        "`prometheus setup --non-interactive` needs every required value from `--set` flags or, for secrets, from the environment (`PMT_SECRET_<NAME>` or the provider's own variable); the message names the missing one.");
    /// A tool category names an unregistered provider.
    TOOLS_UNKNOWN_PROVIDER = ("tools.unknown_provider", Config,
        "The tool configurator (`prometheus tools provider|enable|disable|configure`) named a category, provider or surface that is not registered, or a posture toolset (chosen per session, never saved); the message lists the valid names and nothing was written.");
    /// A toolset's post-setup needs a confirmation nobody can give.
    TOOLS_SETUP_CONFIRMATION = ("tools.setup_confirmation", PermissionDenied,
        "`prometheus tools post-setup <toolset>` would install managed runtimes and needs a confirmation: run it on a terminal or pass `--yes`; nothing was downloaded (the command exits 4).");
    /// `prometheus doctor` was given an unknown group.
    DOCTOR_UNKNOWN_GROUP = ("doctor.unknown_group", Usage,
        "`prometheus doctor <group>` names a group that does not exist; the message lists the groups.");
    /// A slash command needs an admin of the surface.
    SLASH_ADMIN_ONLY = ("slash.admin_only", PermissionDenied,
        "The command (for example `/sessions all` on a messaging platform) is reserved for the surface's admins; nothing was listed or changed.");
    /// An interactive picker ran without a terminal.
    CLI_REQUIRES_TERMINAL = ("cli.requires_terminal", Usage,
        "An interactive command (`sessions browse`, `tools configure`) ran without a terminal on stdin and stdout; use its non-interactive form.");
    /// A slash command collides with a built-in or another source.
    SLASH_CONFLICT = ("slash.conflict", Config,
        "A plugin, skill or quick command declares a slash command whose name or alias is already a built-in command; the built-in is kept and the registration fails.");
    /// The built-in slash command table is inconsistent.
    SLASH_INVALID = ("slash.invalid", Internal,
        "Two built-in slash commands share a name or alias (a programming error caught by the registry test).");
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
    /// The mock provider's script cannot be read.
    MOCK_SCRIPT_MISSING = ("mock.script_missing", NotFound,
        "The script of the built-in `mock` provider (`--mock-script` or `providers.mock.script`) does not exist or cannot be read.");
    /// The mock provider's script is malformed.
    MOCK_SCRIPT_INVALID = ("mock.script_invalid", Config,
        "The script of the built-in `mock` provider is not valid TOML or holds an invalid rule (an unknown field, a bad regex, a fault combined with fields it does not take); the message names the file and the rule.");
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
    /// A tool registration is malformed.
    TOOL_INVALID_REGISTRATION = ("tool.invalid_registration", Config,
        "A tool registration was rejected at load time: its name does not match `^[a-z][a-z0-9_]{0,63}$`, its parameters schema root is not an object, or it names an unknown toolset. The error names the tool and the offending field.");
    /// A tool registration would shadow another owner's tool.
    TOOL_SHADOWED = ("tool.shadowed", Config,
        "A tool name is already registered by another owner (a built-in, plugin or MCP server); the first registration is kept. A plugin replaces a built-in tool only with the grant `tools.override:<tool>`. `prometheus doctor` lists rejected registrations.");
    /// A tool name is not registered in this profile.
    TOOL_UNKNOWN = ("tool.unknown", Usage,
        "No tool of that name is registered in this profile (`unknown tool: <name>`); `prometheus tools list` shows the registered tools.");
    /// `tools list --session` named a session that recorded no tool surface.
    TOOL_SURFACE_UNRECORDED = ("tool.surface_unrecorded", NotFound,
        "The session never recorded a tool surface: it never ran a turn (a surface is frozen when a session builds it) or was created by an import. `prometheus tools list` without `--session` shows the surface a new session would get.");
    /// Toolsets include each other in a cycle.
    TOOLSET_CYCLE = ("toolset.cycle", Config,
        "Toolsets include each other in a cycle (`toolset include cycle: a -> b -> a`); every toolset of the batch that declared it is rejected.");
    /// A command named a toolset that does not exist.
    TOOLSET_UNKNOWN = ("toolset.unknown", Usage,
        "A command named a toolset that does not exist (`unknown toolset '<name>' (did you mean ...?)`); `prometheus toolsets list` shows them. In configuration the same mistake is `registry.unknown_name` (exit 78).");
    /// `approvals test` was given a command line it cannot read.
    APPROVALS_UNPARSABLE = ("approvals.unparsable", Usage,
        "`prometheus approvals test` cannot read the command line (an unterminated quote or substitution); fix its quoting.");
    /// No skill of that name resolves in the profile.
    SKILL_NOT_FOUND = ("skill.not_found", NotFound,
        "No skill of that name resolves in the profile (`Skill '<name>' not found in profile '<profile>'.`); the message names another profile of the home that has it.");
    /// A skill name is claimed twice within one search tier.
    SKILL_AMBIGUOUS = ("skill.ambiguous", InvalidInput,
        "Two different skills share a name within one search tier (`Ambiguous skill name '<name>'` with both paths); rename or remove one of them.");
    /// A skill or a skill write is malformed.
    SKILL_INVALID = ("skill.invalid", InvalidInput,
        "A `SKILL.md` or a skill write is malformed: frontmatter that is not valid YAML or lacks `name`/`description`, a name outside `^[a-z0-9][a-z0-9._-]*$`, an empty body or an oversized file.");
    /// A skill cannot be changed from here.
    SKILL_READ_ONLY = ("skill.read_only", PermissionDenied,
        "The skill cannot be changed: bundled skills are read-only (eject a copy with `prometheus skills eject <name>`), and hub, plugin, project and external-directory skills are not written by Prometheus.");
    /// A skill's security scan refused it or failed.
    SKILL_SCAN_BLOCKED = ("skill.scan_blocked", PermissionDenied,
        "A skill's security scan returned `dangerous` or failed; scanning fails closed, so the skill was not written, loaded or ejected.");
    /// A skill file could not be read or written.
    SKILL_IO = ("skill.io", Io,
        "A skill file or directory could not be read or written.");
    /// The skills hub is not part of this build.
    SKILL_HUB_UNAVAILABLE = ("skill.hub_unavailable", Unavailable,
        "Installing and removing hub skills ships with the skills hub in M2; create skills with `skill_manage` or copy them into the profile's `skills/` directory.");
    /// No pending skill write has that id.
    SKILL_PENDING_NOT_FOUND = ("skill.pending_not_found", NotFound,
        "No staged skill write has that id; `/skills pending` lists them.");
    /// A skill review of the session is already running.
    SKILL_REVIEW_RUNNING = ("skill.review_running", Conflict,
        "A background skill review of this session is already running (`a review is already running`); `/refine` again once it posts its summary.");
    /// A skill bundle file is malformed or names no skill.
    SKILL_BUNDLE_INVALID = ("skill.bundle_invalid", InvalidInput,
        "A skill bundle (`<profile>/skill-bundles/<slug>.toml`) is malformed: TOML that does not parse, an unknown field, an empty `skills` list or more than 32 entries; `prometheus skills bundles list` reports each file.");
    /// No skill bundle has that slug.
    SKILL_BUNDLE_NOT_FOUND = ("skill.bundle_not_found", NotFound,
        "No skill bundle has that slug; `prometheus skills bundles list` lists them.");
    /// No memory entry has that id.
    MEMORY_NOT_FOUND = ("memory.not_found", NotFound,
        "No memory entry of the profile has that id; `prometheus memory list --all` shows every entry with its id.");
    /// A memory write or a memory file is malformed.
    MEMORY_INVALID = ("memory.invalid", InvalidInput,
        "A memory write, import file or edited Markdown view is malformed: an unknown target, kind or tier, an empty entry, an `expires_at` that is not a future RFC 3339 timestamp, or a section heading that names no known entry.");
    /// A memory write would exceed the core tier's character budget.
    MEMORY_OVER_BUDGET = ("memory.over_budget", InvalidInput,
        "The write would push the target's core tier past `memory.memory_char_limit` or `memory.user_char_limit` (or the archival tier past `memory.archival_max_entries`); consolidate entries in one batch or store the entry with `tier: \"archival\"`.");
    /// A memory write was refused by the threat or secret scan.
    MEMORY_BLOCKED = ("memory.blocked", PermissionDenied,
        "The entry matches a prompt-injection or secret-exfiltration pattern, or contains a credential; memory never stores it.");
    /// No pending memory change has that id.
    MEMORY_PENDING_NOT_FOUND = ("memory.pending_not_found", NotFound,
        "No staged memory change has that id; `prometheus memory pending` lists them.");
    /// A staged memory change no longer matches the entry it was proposed against.
    MEMORY_STALE = ("memory.stale", Conflict,
        "The staged change was proposed against an entry that has since changed or been removed; reject it and let the agent propose again.");
    /// The built-in memory store is disabled for that target.
    MEMORY_DISABLED = ("memory.disabled", Config,
        "The target's built-in store is disabled (`memory.memory_enabled` / `memory.user_profile_enabled`); enable it with `prometheus config set`.");
    /// A release file's signature does not verify against the embedded keys.
    UPDATE_SIGNATURE = ("update.signature", PermissionDenied,
        "A downloaded release index, manifest or archive is unsigned, or its minisign signature does not verify against the release keys embedded in this binary (primary or standby), or its bytes do not match the signed manifest; nothing was installed. There is no flag that skips this check.");
    /// The install is updated by something else.
    UPDATE_OWNED_ELSEWHERE = ("update.owned_elsewhere", Unavailable,
        "This install's receipt names another owner (a package manager, the container image, or a source build): `prometheus update` refuses before any network request and names the command that updates it.");
    /// Another update holds the update lock.
    UPDATE_LOCKED = ("update.locked", Conflict,
        "Another `prometheus update` holds `<PROMETHEUS_HOME>/updates/update.lock`; nothing was changed. Wait for it to finish.");
    /// An update step failed.
    UPDATE_FAILED = ("update.failed", Tool,
        "The update could not complete (release server unreachable, malformed release metadata, no archive for this target, or a filesystem error); the installed binary is unchanged.");
    /// The staged build failed its self-test.
    UPDATE_SELF_TEST_FAILED = ("update.self_test_failed", Tool,
        "The downloaded binary failed `prometheus self-test` (version, schema versions, sandbox probe, profile configs) within 30 s; it was not installed.");
    /// `update --rollback` cannot run.
    UPDATE_ROLLBACK_REFUSED = ("update.rollback_refused", Tool,
        "There is no `prometheus.prev` to roll back to, or a profile's storage was migrated beyond what the previous binary reads; rerun with `--restore-snapshots` to restore the pre-update snapshots.");
    /// A version or channel argument is malformed.
    UPDATE_INVALID_ARGUMENT = ("update.invalid_argument", Usage,
        "A release version is not `vMAJOR.MINOR.PATCH` (or `vMAJOR.MINOR.PATCH-canary.YYYYMMDDHHMMSS`), or a channel is not `stable` or `canary`.");
    /// A `state.db` was written by a build this one cannot read.
    STORE_SCHEMA_UNREADABLE = ("store.schema_unreadable", Tool,
        "`state.db` was migrated by a newer Prometheus to a schema this build cannot read; it was not modified. Update Prometheus, or roll back with `prometheus update --rollback --restore-snapshots`.");
    /// The pre-update backup could not be written.
    UPDATE_BACKUP_FAILED = ("update.backup_failed", Io,
        "The pre-update backup (`update.pre_update_backup`, `--backup`) could not be written, usually because the home volume is full; the update stopped before the swap (exit 73) and the installed binary is unchanged. Free space, or rerun with `--no-backup`.");
    /// `prometheus demo --output` names a directory that is not empty.
    DEMO_OUTPUT_NOT_EMPTY = ("demo.output_not_empty", Usage,
        "`prometheus demo --output DIR` names a directory that is not empty; the demo only runs in a new or empty directory, so it never removes files it did not create.");
    /// The demo could not write its directory.
    DEMO_IO = ("demo.io", Io,
        "`prometheus demo` could not create its directory, home, ledger repository or mock script.");
    /// A runtime name is not in the pinned runtime index.
    RUNTIME_UNKNOWN = ("runtime.unknown", Usage,
        "The name is not a runtime of this build's pinned runtime index; `prometheus runtimes list` shows the pinned names.");
    /// A runtime has no pinned artifact for this host.
    RUNTIME_UNSUPPORTED = ("runtime.unsupported", Unavailable,
        "The pinned runtime index has no archive of this runtime for this host's OS, architecture and C library.");
    /// A downloaded runtime archive does not match its pinned digest.
    RUNTIME_DIGEST_MISMATCH = ("runtime.digest_mismatch", PermissionDenied,
        "The downloaded archive's size or SHA-256 differs from the digest pinned in this binary (a corrupted download, or a mirror serving other bytes); the download was deleted and nothing was installed.");
    /// A download failed after the retry budget.
    RUNTIME_DOWNLOAD = ("runtime.download", Unavailable,
        "The download failed with a permanent error, or with transient errors past the retry budget (4 consecutive failures without progress). Check the network, `network.proxy`, or the configured mirror.");
    /// A runtime is excluded by `runtimes.exclude`.
    RUNTIME_EXCLUDED = ("runtime.excluded", Tool,
        "The runtime is listed in `runtimes.exclude` in `daemon.toml`, so nothing installs it; `prometheus runtimes exclude remove <name>` allows it again.");
    /// A runtime is missing and lazy installs are disabled.
    RUNTIME_NOT_INSTALLED = ("runtime.not_installed", Unavailable,
        "The runtime is not in the store and `runtimes.allow_lazy_install = false`; install it with `prometheus runtimes install <name>`.");
    /// The user declined a runtime install.
    RUNTIME_DECLINED = ("runtime.declined", Cancelled,
        "The install prompt was answered no; nothing was downloaded.");
    /// A runtime store file could not be read or written.
    RUNTIME_IO = ("runtime.io", Io,
        "A file under `<PROMETHEUS_HOME>/runtimes/` could not be read, written or renamed.");
    /// A client frame is not JSON (JSON-RPC -32700).
    RPC_PARSE = ("rpc.parse", InvalidInput,
        "A client protocol frame is not valid JSON (JSON-RPC `-32700`); the connection keeps serving.");
    /// A frame is JSON but not a JSON-RPC 2.0 request (-32600).
    RPC_INVALID_REQUEST = ("rpc.invalid_request", InvalidInput,
        "A client protocol frame is JSON but not a JSON-RPC 2.0 request object (JSON-RPC `-32600`).");
    /// The method is not in the catalog (-32601).
    RPC_UNKNOWN_METHOD = ("rpc.unknown_method", Usage,
        "The called method is not declared in the RPC catalog (`schema/rpc.json`); JSON-RPC `-32601`.");
    /// Params name a field the method does not declare (-32602).
    RPC_UNKNOWN_FIELD = ("rpc.unknown_field", InvalidInput,
        "The params name a field the method's contract does not declare; `data.path` is its JSON pointer (JSON-RPC `-32602`).");
    /// Params miss a field or have the wrong type (-32602).
    RPC_INVALID_PARAMS = ("rpc.invalid_params", InvalidInput,
        "The params are not an object, miss a required field or have a value of the wrong type; `data.path` points at it (JSON-RPC `-32602`).");
    /// A request came before `rpc.hello`.
    RPC_HANDSHAKE_REQUIRED = ("rpc.handshake_required", Protocol,
        "The first request on a connection must be `rpc.hello`.");
    /// Client and daemon speak different protocol majors.
    RPC_PROTOCOL_MISMATCH = ("rpc.protocol_mismatch", Protocol,
        "The client and the daemon speak different major versions of the client protocol; update the older side (`prometheus update`).");
    /// The method is declared by a capability this build does not serve yet.
    RPC_NOT_SERVED = ("rpc.not_served", Unavailable,
        "The method is declared in the RPC catalog by a capability this daemon does not serve yet.");
    /// The daemon produced a result or payload that breaks its contract.
    RPC_CONTRACT_VIOLATION = ("rpc.contract_violation", Internal,
        "The daemon produced a result or event payload that does not match its contract (a bug; it is logged and counted in `prometheus.rpc.contract_violations`).");
    /// A server-to-client request is no longer open.
    RPC_REQUEST_CLOSED = ("rpc.request_closed", NotFound,
        "The answered server-to-client request is not open any more (answered elsewhere, timed out or cancelled); the answer was dropped.");
    /// The daemon is restarting and accepts no new work.
    RPC_RETIRING = ("rpc.retiring", Unavailable,
        "The daemon is restarting and refuses new work; reconnect to continue.");
    /// A session is not visible to, or not attached by, the connection.
    RPC_SESSION_DENIED = ("rpc.session_denied", PermissionDenied,
        "The session is not one of the connection's profile, or a mutating method was called without attaching to it; the error does not say whether the session exists elsewhere.");
    /// The connection to the daemon failed or closed.
    RPC_CONNECTION = ("rpc.connection", Unavailable,
        "The connection to the daemon could not be opened or was closed (the WebSocket close code is in the message).");
    /// No ticket, an unknown, expired or reused one, or a bad request on the control channel.
    RPC_TICKET = ("rpc.ticket", Auth,
        "The control channel refused the ticket request (unknown profile or purpose, several profiles for a purpose other than `profile-admin`).");
    /// No daemon runs for the home.
    DAEMON_NOT_RUNNING = ("daemon.not_running", Unavailable,
        "No gateway daemon runs for this home; start it with `prometheus daemon start`.");
    /// The daemon did not become ready.
    DAEMON_START_FAILED = ("daemon.start_failed", Unavailable,
        "`prometheus daemon start` launched the daemon but it exited or did not publish its endpoint in time; see `prometheus logs daemon` and the exit status in the message.");
    /// No profile of the home can be served.
    DAEMON_NO_PROFILE = ("daemon.no_profile", Config,
        "Every profile of the home failed to load (each error is logged), so the daemon has nothing to serve.");
    /// A turn is already running in the session.
    SESSION_BUSY = ("session.busy", Conflict,
        "The session is running a turn; submit again when it finishes.");
    /// A session method on a connection not attached to the session.
    SESSION_NOT_ATTACHED = ("session.not_attached", PermissionDenied,
        "Every session-addressed method needs the connection to attach to the session first (`session.attach`); the error does not say whether the session exists.");
    /// A profile the connection's ticket does not grant.
    PROFILE_NOT_GRANTED = ("profile.not_granted", PermissionDenied,
        "The request names a profile the connection's ticket does not grant; get a ticket for that profile.");
    /// An input carried a client-supplied author.
    RPC_FORGED_AUTHOR = ("rpc.forged_author", PermissionDenied,
        "`prompt.submit` carried an `author`; the gateway stamps every input's author from the connection and never takes one from a client.");
    /// `session.create` named another client kind as its source.
    RPC_SOURCE_MISMATCH = ("rpc.source_mismatch", PermissionDenied,
        "`session.create` named a `source` other than the connection's own client kind (from `rpc.hello`).");
    /// An observer tried to drive the session.
    SESSION_OBSERVER = ("session.observer", PermissionDenied,
        "The connection attached as an observer, which only reads; attach as a driver to submit, interrupt or answer.");
    /// An answer to a server request that already settled.
    RPC_REQUEST_SETTLED = ("rpc.request_settled", Conflict,
        "The server-to-client request already settled (answered, timed out or cancelled); the late answer changed nothing.");
    /// A request handler failed unexpectedly.
    RPC_INTERNAL = ("rpc.internal", Internal,
        "The daemon's handler for the method failed unexpectedly (a crash report was written); the connection stays open.");
    /// `prometheus tui` without an interactive terminal.
    TUI_NOT_A_TERMINAL = ("tui.not_a_terminal", Usage,
        "`prometheus tui` needs stdin and stdout to be terminals; use `prometheus chat` (line mode or one-shot) when piping.");
    /// A pairing approval while pairing is locked after failed attempts.
    GATEWAY_PAIRING_LOCKED = ("gateway.pairing_locked", PermissionDenied,
        "Five failed pairing approvals locked pairing for that platform for an hour; wait until the stated time.");
    /// A pairing code that matches no pending request.
    GATEWAY_PAIRING_UNKNOWN = ("gateway.pairing_unknown", NotFound,
        "No pending pairing request has that code (it may have expired after an hour); ask the sender to message the bot again for a new code.");
    /// An approved sender or pending request that does not exist.
    GATEWAY_PAIRING_NOT_FOUND = ("gateway.pairing_not_found", NotFound,
        "No pairing entry matches; `prometheus pairing list` shows pending and approved entries.");
    /// An outbox entry that does not exist or is not in a state the action applies to.
    GATEWAY_OUTBOX_ENTRY = ("gateway.outbox_entry", NotFound,
        "No outbox entry with that id is in a state the action applies to; `prometheus gateway outbox list` shows the entries.");
    /// The clean-shutdown marker exists but cannot be removed.
    GATEWAY_RECOVERY = ("gateway.recovery", Io,
        "The daemon could not remove `<home>/run/clean_shutdown`, so it cannot tell whether the last stop was clean; fix the file's permissions and start again.");
    /// `gateway whatsapp pair` linked nothing in time.
    GATEWAY_PAIRING_TIMEOUT = ("gateway.pairing_timeout", Conflict,
        "The phone did not link within the pairing window (120 seconds); run `prometheus gateway whatsapp pair` again and scan the newest QR code (or use `--pairing-code <phone>`).");
    /// A messaging delivery with no adapter for its platform.
    GATEWAY_NO_ADAPTER = ("gateway.no_adapter", Unavailable,
        "The profile has no running adapter for the platform; enable it under `[gateway.platforms.<name>]` and run `prometheus gateway reload`.");
    /// Every messaging adapter failed for good with `on_all_adapters_down = "exit"`.
    GATEWAY_ALL_ADAPTERS_DOWN = ("gateway.all_adapters_down", Config,
        "Every messaging adapter stopped with a non-retryable error and `gateway.on_all_adapters_down = \"exit\"`; fix the adapters' configuration or credentials, or set it to `stay_alive`.");
    /// A new turn while the home is paused.
    PAUSE_ACTIVE = ("pause.active", Unavailable,
        "The home is paused (`<home>/run/paused`): no new turns or scheduled work start until `prometheus pause off` (or `/pause off` from an admin chat).");
    /// A delivery to a platform without a home channel.
    GATEWAY_NO_HOME_CHANNEL = ("gateway.no_home_channel", NotFound,
        "The delivery names only a platform and that platform has no home channel; run /sethome in the target chat or name the chat (`platform:chat_id`).");
    /// A send target that is malformed or names nothing known.
    GATEWAY_UNKNOWN_TARGET = ("gateway.unknown_target", NotFound,
        "The target is not `platform`, `platform:chat_id`, `platform:chat_id:thread_id` or `platform:#channel-name`, or names a channel nobody knows; `prometheus gateway send --list` shows the known targets.");
    /// A send to a platform the profile does not enable.
    GATEWAY_PLATFORM_NOT_CONFIGURED = ("gateway.platform_not_configured", Config,
        "The profile does not enable that platform; add `[gateway.platforms.<name>]` with `enabled = true` and its credential.");
    /// A one-shot send that needs the daemon.
    GATEWAY_DAEMON_REQUIRED = ("gateway.daemon_required", Unavailable,
        "The platform's adapter cannot send without a running connection; start the daemon with `prometheus daemon start` and send again.");
    /// A one-shot send the platform refused or that was not delivered in time.
    GATEWAY_DELIVERY_FAILED = ("gateway.delivery_failed", Tool,
        "The message was not delivered; `prometheus gateway outbox list` shows the entry and its last error.");
    /// A destination the `send_message` tool may not reach.
    GATEWAY_TARGET_REFUSED = ("gateway.target_refused", PermissionDenied,
        "The tool may send only to a platform's home channel, a chat in `allow_chats` or a chat the profile already has a session in.");
    /// A relay enrollment token the connector refused.
    GATEWAY_ENROLL_REJECTED = ("gateway.enroll_rejected", Config,
        "The relay connector refused the enrollment token (already used, expired or unknown); ask the relay operator for a new one-time token.");
    /// The relay connector could not be reached or answered nonsense.
    GATEWAY_RELAY = ("gateway.relay", Unavailable,
        "The relay connector could not be reached over TLS or sent an unexpected answer; check `gateway.relay.url` (or `--connector-url`) and the network.");
    /// A plugin directory given to `plugins test-adapter` needs the WASM
    /// plugin host, which this build does not have yet.
    PLUGINS_HOST_UNAVAILABLE = ("plugins.host_unavailable", Unavailable,
        "The command needs the WASM plugin host to load a plugin adapter, and this build has none; first-party adapters are tested by platform name (`prometheus plugins test-adapter telegram`) in a build with the `adapter-kits` feature.");
    /// A `plugin.toml` (or a WIT world it names) is invalid.
    PLUGINS_MANIFEST = ("plugins.manifest", InvalidInput,
        "A plugin manifest is invalid: an unknown field, kind, world or capability, a newer manifest_version, a requires_prometheus range excluding this version, or a bad value; the message names it.");
    /// No discovered plugin has this id.
    PLUGINS_NOT_FOUND = ("plugins.not_found", NotFound,
        "No plugin with this id was discovered in the profile's plugins directory, a trusted project directory or the built-in plugins; `prometheus plugins list` shows what was found.");
    /// A plugin directory cannot be used (reserved id, unreadable files).
    PLUGINS_DISCOVERY = ("plugins.discovery", Config,
        "A plugin directory cannot be used: its id is reserved by a built-in plugin, it does not match its directory name, or its files cannot be read; the message names it.");
    /// Enabling needs the user to approve the plugin's capabilities.
    PLUGINS_APPROVAL_REQUIRED = ("plugins.approval_required", PermissionDenied,
        "Enabling a plugin needs its capabilities approved; run the command on a terminal, or pass `--grant all` or `--grant <capability>,...` to approve non-interactively.");
    /// A plugin used a capability its grant does not cover.
    PLUGINS_CAPABILITY_DENIED = ("plugins.capability_denied", PermissionDenied,
        "A plugin called a host interface its approved capabilities do not cover (`capability_denied: <capability> <detail>`); approve it with `prometheus plugins grants approve <id> <capability>` if intended.");
    /// A plugin could not be compiled, linked or instantiated.
    PLUGINS_LOAD = ("plugins.load", Unavailable,
        "A plugin could not be loaded: its component is missing or invalid, imports something the host does not offer, lacks an export of a world it declares, or failed while registering; the message names the plugin.");
    /// A call into a plugin trapped, timed out or returned an error.
    PLUGINS_CALL = ("plugins.call", Tool,
        "A call into a plugin failed: it trapped, exceeded `plugins.wasm.call_timeout_ms`, is suspended after repeated traps, or returned an error; the message names the plugin.");
    /// A plugin package without a signature the policy accepts.
    PLUGINS_UNSIGNED = ("plugins.unsigned", PermissionDenied,
        "The plugin package carries no signature a trusted key verifies; under `plugins.signature_policy = \"require\"` (and for every catalog install) it installs only with `--allow-unsigned`, which the install record keeps.");
    /// A plugin package or catalog whose signature does not verify.
    PLUGINS_SIGNATURE = ("plugins.signature", PermissionDenied,
        "The plugin package's signature does not verify against its publisher key (or the package does not match its catalog digest); nothing was installed, and no flag overrides it.");
    /// A plugin on the catalog's kill list.
    PLUGINS_REMOVED = ("plugins.removed", PermissionDenied,
        "The plugin (or its repository) was removed from the catalog and is blocked; only `prometheus plugins install --allow-removed` installs it anyway, recorded on its install record.");
    /// A plugin the install-time threat scan refused.
    PLUGINS_THREAT = ("plugins.threat", PermissionDenied,
        "The install-time threat scan graded the plugin `dangerous` (refused; `--force` does not override it), graded it `caution` without confirmation or `--force`, or could not finish (it fails closed); the message names each finding, file and line.");
    /// A plugin catalog or package that could not be fetched.
    PLUGINS_CATALOG = ("plugins.catalog", Unavailable,
        "The plugin catalog entry, package or repository could not be fetched or read; the message names it.");
    /// No MCP server with this name is configured.
    MCP_NOT_FOUND = ("mcp.not_found", NotFound,
        "No `[mcp.servers.<name>]` entry has this name in the profile's config.toml; `prometheus mcp list` shows the configured servers.");
    /// `mcp add` of a name that is already configured.
    MCP_EXISTS = ("mcp.exists", InvalidInput,
        "An MCP server with this name is already configured; remove it first with `prometheus mcp remove <name>` or pick another name.");
    /// An MCP server could not be started or reached.
    MCP_UNREACHABLE = ("mcp.unreachable", Unavailable,
        "The MCP server did not start, did not complete the initialize handshake or the tool listing within its connect timeout, or refused the connection; the message says why.");
    /// `mcp login` of a server whose authorization server registers no
    /// clients dynamically, without a configured client.
    MCP_OAUTH_CLIENT_REQUIRED = ("mcp.oauth_client_required", Config,
        "The MCP server's authorization server does not register OAuth clients dynamically (and accepts no client metadata document); register a client with its provider and set `mcp.servers.<name>.oauth.client_id` (and `oauth.client_secret_name`).");
    /// `mcp login` failed.
    MCP_LOGIN_FAILED = ("mcp.login_failed", Auth,
        "The MCP server's OAuth login did not complete: discovery, registration, the authorization response (state or issuer mismatch, refusal) or the token exchange failed, or no one answered in time; the message says which.");
    /// `mcp serve --http` on an address that is not loopback.
    MCP_REMOTE_REFUSED = ("mcp.remote_refused", PermissionDenied,
        "`prometheus mcp serve --http` binds only loopback addresses unless `mcp_server.allow_remote = true` is set in the profile's config.toml.");
    /// Connectors without a Nous sign-in.
    CONNECTORS_SIGNIN_REQUIRED = ("connectors.signin_required", Config,
        "Hosted connectors run on the Nous connector gateway and need a Nous Portal sign-in in this profile: run `prometheus auth login nous`. No identity is ever created without that step.");
    /// Connectors switched off.
    CONNECTORS_DISABLED = ("connectors.disabled", Config,
        "`connectors.enabled = false` in the profile's config.toml; set it to true to use hosted connectors.");
    /// An account alias outside the alias pattern.
    CONNECTORS_INVALID_ALIAS = ("connectors.invalid_alias", Usage,
        "A connector account alias must match `^[a-z0-9][a-z0-9-]{0,31}$` (lower-case letters, digits and dashes, at most 32 characters, not starting with a dash).");
    /// No such connector or account.
    CONNECTORS_NOT_FOUND = ("connectors.not_found", NotFound,
        "The connector gateway knows no connector or account by this name; `prometheus connectors list` and `prometheus connectors accounts` show what exists.");
    /// The gateway refused or could not be reached.
    CONNECTORS_GATEWAY = ("connectors.gateway", Unavailable,
        "The Nous connector gateway could not be reached, answered an error or does not serve connectors for this account; the message carries its answer.");
    /// A connection the user did not finish in time.
    CONNECTORS_NOT_CONNECTED = ("connectors.not_connected", Unavailable,
        "`prometheus connectors connect` waited `connectors.connect_timeout_secs` and the account was still not connected; open the link again or rerun the command.");
    /// A disconnect nobody confirmed.
    CONNECTORS_CONFIRMATION = ("connectors.confirmation", PermissionDenied,
        "`prometheus connectors disconnect` removes an account and asks for confirmation: run it on a terminal or pass `--yes`; nothing was removed (the command exits 4).");
    /// A system-scope service install without root or administrator rights.
    SERVICE_NEEDS_ROOT = ("service.needs_root", PermissionDenied,
        "`prometheus daemon install-service --system` installs a boot-time unit and must run as root (administrator on Windows); the per-user install needs no elevation.");
    /// The OS service manager refused or is missing.
    SERVICE_MANAGER = ("service.manager", Unavailable,
        "The OS service manager (systemd, launchd or Task Scheduler) refused the operation or is not available; the message carries its output.");
    /// No supported service manager on this OS.
    SERVICE_UNSUPPORTED = ("service.unsupported", Unavailable,
        "`prometheus daemon install-service` knows systemd (Linux), launchd (macOS) and Task Scheduler (Windows); on other OSes (Android/Termux, the BSDs) run `prometheus daemon run` under your own supervisor.");
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

    /// An I/O failure on `path`: `<path>: <err>` under `code`, keeping the
    /// I/O error (its kind and OS code) as the source.
    #[must_use]
    pub fn io(code: &'static ErrorCode, path: &std::path::Path, err: &std::io::Error) -> Self {
        Self::new(code, format!("{}: {err}", path.display())).with_source(copy_io(err))
    }

    /// [`Error::io`] saying what failed: `<doing> <path>: <err>`, e.g.
    /// `cannot write /x: ...`.
    #[must_use]
    pub fn io_doing(
        code: &'static ErrorCode,
        doing: &str,
        path: &std::path::Path,
        err: &std::io::Error,
    ) -> Self {
        Self::new(code, format!("{doing} {}: {err}", path.display())).with_source(copy_io(err))
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

/// An owned copy of `err` for the source chain (`io::Error` is not
/// `Clone`): the OS error when there is one, else its kind and message.
fn copy_io(err: &std::io::Error) -> std::io::Error {
    err.raw_os_error().map_or_else(
        || std::io::Error::new(err.kind(), err.to_string()),
        std::io::Error::from_raw_os_error,
    )
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

    #[test]
    fn an_io_error_names_the_path_and_keeps_the_io_error_as_source() {
        let err = Error::io_doing(
            &codes::HOME_IO,
            "cannot write",
            std::path::Path::new("/x/config.toml"),
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(err.message().starts_with("cannot write /x/config.toml: "));
        let source = std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind);
        assert_eq!(source, Some(std::io::ErrorKind::PermissionDenied));
    }
}

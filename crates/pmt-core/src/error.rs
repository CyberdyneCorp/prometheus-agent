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

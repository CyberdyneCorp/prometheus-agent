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

// The catalog itself (one `error_codes!` invocation) lives in its own
// file: it is long, and every new capability adds to it.
include!("error_catalog.rs");

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
mod tests;

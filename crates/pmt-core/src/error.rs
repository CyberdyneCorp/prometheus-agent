//! The error taxonomy: every user-visible error has exactly one kind from
//! [`ERROR_KINDS`] and a stable dotted code from [`ERROR_CODES`]
//! (spec: workspace-architecture, Error taxonomy).

/// The closed set of error kinds a user-visible error may carry.
///
/// The error type itself and the kind-to-exit-code mapping are expanded in
/// M1 group 4 (task 4.1).
pub const ERROR_KINDS: &[&str] = &[
    "usage",
    "invalid_input",
    "not_found",
    "config",
    "auth",
    "permission_denied",
    "budget_exceeded",
    "provider",
    "tool",
    "sandbox",
    "conflict",
    "unavailable",
    "protocol",
    "cancelled",
    "io",
    "internal",
];

/// One entry of the error-code catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode {
    /// Stable dotted code, for example `config.unknown_key`.
    pub code: &'static str,
    /// One of [`ERROR_KINDS`].
    pub kind: &'static str,
    /// What the error means, as documented for users.
    pub summary: &'static str,
}

/// Every registered error code. Empty until the first capability that
/// reports errors lands; codes are added in M1 group 4 onwards.
pub const ERROR_CODES: &[ErrorCode] = &[];

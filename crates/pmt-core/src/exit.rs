//! The process exit-code table: the single definition that every subcommand
//! uses and that the `cli` and `distribution-and-update` specs restate
//! (design.md, Shared contracts).

/// One process exit code with the outcome it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode {
    /// Numeric process status.
    pub code: u8,
    /// Short symbolic name, stable across releases.
    pub name: &'static str,
    /// The outcome the code reports, as documented for users.
    pub meaning: &'static str,
}

const fn exit(code: u8, name: &'static str, meaning: &'static str) -> ExitCode {
    ExitCode {
        code,
        name,
        meaning,
    }
}

// The named statuses of `EXIT_CODES`: commands return these, never a
// literal, so the table stays the one definition.

/// Success.
pub const SUCCESS: u8 = 0;
/// The command ran but reported failure.
pub const FAILURE: u8 = 1;
/// Usage error.
pub const USAGE: u8 = 2;
/// Budget exhausted.
pub const BUDGET_EXHAUSTED: u8 = 3;
/// Approval or confirmation required but unobtainable.
pub const APPROVAL_REQUIRED: u8 = 4;
/// Partial result.
pub const PARTIAL: u8 = 5;
/// Update available (`update --check`).
pub const UPDATE_AVAILABLE: u8 = 10;
/// Invalid input data.
pub const INVALID_INPUT: u8 = 65;
/// Not found.
pub const NOT_FOUND: u8 = 66;
/// Unavailable.
pub const UNAVAILABLE: u8 = 69;
/// Internal error (also a panic on the main thread).
pub const INTERNAL: u8 = 70;
/// Cannot create output.
pub const CANNOT_CREATE_OUTPUT: u8 = 73;
/// I/O error.
pub const IO: u8 = 74;
/// Temporary failure or lock held.
pub const TEMPORARY_FAILURE: u8 = 75;
/// Protocol mismatch.
pub const PROTOCOL_MISMATCH: u8 = 76;
/// Permission denied or strict security posture failure.
pub const PERMISSION_DENIED: u8 = 77;
/// Configuration or credential error.
pub const CONFIG: u8 = 78;
/// Interrupted by SIGINT.
pub const INTERRUPTED: u8 = 130;
/// Terminated by SIGTERM.
pub const TERMINATED: u8 = 143;

/// Every exit code the binary may return, in ascending order. Each
/// [`ErrorKind`](crate::error::ErrorKind) maps to one of them.
pub const EXIT_CODES: &[ExitCode] = &[
    exit(SUCCESS, "success", "Success."),
    exit(
        FAILURE,
        "failure",
        "The command ran but reported failure (agent error, failing eval tasks, doctor failures).",
    ),
    exit(
        USAGE,
        "usage",
        "Usage error (unknown command, flag or argument).",
    ),
    exit(BUDGET_EXHAUSTED, "budget_exhausted", "Budget exhausted."),
    exit(
        APPROVAL_REQUIRED,
        "approval_required",
        "Approval or confirmation required but unobtainable non-interactively.",
    ),
    exit(
        PARTIAL,
        "partial",
        "Partial result (some items skipped or ungraded).",
    ),
    exit(
        UPDATE_AVAILABLE,
        "update_available",
        "Update available (`prometheus update --check` only).",
    ),
    exit(
        INVALID_INPUT,
        "invalid_input",
        "Invalid input data (for example harness tree validation).",
    ),
    exit(NOT_FOUND, "not_found", "Not found."),
    exit(
        UNAVAILABLE,
        "unavailable",
        "Unavailable (provider, daemon, runtime, surface missing, or self-update of an install owned elsewhere).",
    ),
    exit(INTERNAL, "internal", "Internal error."),
    exit(
        CANNOT_CREATE_OUTPUT,
        "cannot_create_output",
        "Cannot create output.",
    ),
    exit(IO, "io", "I/O error."),
    exit(
        TEMPORARY_FAILURE,
        "temporary_failure",
        "Temporary failure or lock held (retryable).",
    ),
    exit(PROTOCOL_MISMATCH, "protocol_mismatch", "Protocol mismatch."),
    exit(
        PERMISSION_DENIED,
        "permission_denied",
        "Permission denied or strict security posture failure.",
    ),
    exit(CONFIG, "config", "Configuration or credential error."),
    exit(INTERRUPTED, "interrupted", "Interrupted by SIGINT."),
    exit(TERMINATED, "terminated", "Terminated by SIGTERM."),
];

/// The table entry for a numeric status, if it is a documented code.
#[must_use]
pub fn lookup(code: u8) -> Option<&'static ExitCode> {
    EXIT_CODES.iter().find(|e| e.code == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_constants_are_documented() {
        let named = [
            (SUCCESS, "success"),
            (FAILURE, "failure"),
            (USAGE, "usage"),
            (BUDGET_EXHAUSTED, "budget_exhausted"),
            (APPROVAL_REQUIRED, "approval_required"),
            (PARTIAL, "partial"),
            (UPDATE_AVAILABLE, "update_available"),
            (INVALID_INPUT, "invalid_input"),
            (NOT_FOUND, "not_found"),
            (UNAVAILABLE, "unavailable"),
            (INTERNAL, "internal"),
            (CANNOT_CREATE_OUTPUT, "cannot_create_output"),
            (IO, "io"),
            (TEMPORARY_FAILURE, "temporary_failure"),
            (PROTOCOL_MISMATCH, "protocol_mismatch"),
            (PERMISSION_DENIED, "permission_denied"),
            (CONFIG, "config"),
            (INTERRUPTED, "interrupted"),
            (TERMINATED, "terminated"),
        ];
        for (code, name) in named {
            assert_eq!(lookup(code).map(|e| e.name), Some(name));
        }
        assert_eq!(named.len(), EXIT_CODES.len());
        assert!(lookup(99).is_none());
    }

    #[test]
    fn table_matches_the_shared_contract() {
        let codes: Vec<u8> = EXIT_CODES.iter().map(|e| e.code).collect();
        assert_eq!(
            codes,
            [
                0, 1, 2, 3, 4, 5, 10, 65, 66, 69, 70, 73, 74, 75, 76, 77, 78, 130, 143
            ]
        );
    }
}

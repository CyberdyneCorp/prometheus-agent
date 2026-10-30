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

/// Every exit code the binary may return, in ascending order.
///
/// The mapping from each error kind to one of these codes is expanded in
/// M1 group 4 (task 4.1).
pub const EXIT_CODES: &[ExitCode] = &[
    exit(0, "success", "Success."),
    exit(
        1,
        "failure",
        "The command ran but reported failure (agent error, failing eval tasks, doctor failures).",
    ),
    exit(
        2,
        "usage",
        "Usage error (unknown command, flag or argument).",
    ),
    exit(3, "budget_exhausted", "Budget exhausted."),
    exit(
        4,
        "approval_required",
        "Approval or confirmation required but unobtainable non-interactively.",
    ),
    exit(
        5,
        "partial",
        "Partial result (some items skipped or ungraded).",
    ),
    exit(
        10,
        "update_available",
        "Update available (`prometheus update --check` only).",
    ),
    exit(
        65,
        "invalid_input",
        "Invalid input data (for example harness tree validation).",
    ),
    exit(66, "not_found", "Not found."),
    exit(
        69,
        "unavailable",
        "Unavailable (provider, daemon, runtime, surface missing, or self-update of an install owned elsewhere).",
    ),
    exit(70, "internal", "Internal error."),
    exit(73, "cannot_create_output", "Cannot create output."),
    exit(74, "io", "I/O error."),
    exit(
        75,
        "temporary_failure",
        "Temporary failure or lock held (retryable).",
    ),
    exit(76, "protocol_mismatch", "Protocol mismatch."),
    exit(
        77,
        "permission_denied",
        "Permission denied or strict security posture failure.",
    ),
    exit(78, "config", "Configuration or credential error."),
    exit(130, "interrupted", "Interrupted by SIGINT."),
    exit(143, "terminated", "Terminated by SIGTERM."),
];

#[cfg(test)]
mod tests {
    use super::EXIT_CODES;

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

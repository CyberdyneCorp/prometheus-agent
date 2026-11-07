//! Where ended processes leave their receipts (spec: execution-
//! environments, Retained process receipts). The host keeps them in the
//! profile's `state.db`, scoped to the conversation of the session that
//! ran the process; this crate only reads and writes through [`Receipts`].

/// What an ended background process left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The process id the model knew it by.
    pub process_id: String,
    /// Its command line.
    pub command: String,
    /// Its exit code (-1 when killed).
    pub exit_code: i32,
    /// Start, Unix milliseconds.
    pub started_at_ms: i64,
    /// End, Unix milliseconds.
    pub ended_at_ms: i64,
    /// The last characters of its redacted output.
    pub output: String,
}

/// The receipts of one session's conversation.
pub trait Receipts: Send + Sync {
    /// Keeps `receipt` (failures are logged, never surfaced).
    fn keep(&self, receipt: Receipt);
    /// The conversation's receipts whose id starts with `prefix`.
    fn matching(&self, prefix: &str) -> Vec<Receipt>;
    /// Every receipt of the conversation, newest first.
    fn all(&self) -> Vec<Receipt>;
}

/// Unix milliseconds now.
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

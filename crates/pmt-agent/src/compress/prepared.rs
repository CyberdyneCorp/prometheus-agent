//! Prepared-ahead summaries (spec: context-compression, Prepared-ahead
//! summaries).
//!
//! With `compression.prepare_ahead`, once the context passes 90% of the
//! trigger the summary of the would-be middle is written in the background.
//! When the trigger is crossed it is adopted only if every row it was
//! prepared against is still there unchanged and in order, rows having only
//! been appended since: the compaction then summarizes exactly the prepared
//! middle (the new rows join the tail) without another summarizer call.
//! Anything else discards it and the compaction summarizes as usual.

use pmt_core::id::MessageId;
use pmt_store::Message as Row;
use tokio::task::JoinHandle;

use super::compactor::Plan;
use super::split::Split;
use super::summary::{FailureKind, SummaryFailure};

/// A summary being written ahead of the trigger.
pub struct Prepared {
    /// The active rows it was prepared against: id and content hash.
    rows: Vec<(MessageId, String)>,
    split: Split,
    summary: JoinHandle<Result<String, SummaryFailure>>,
}

/// An adopted summary (awaited if still being written).
pub struct PreparedSummary(JoinHandle<Result<String, SummaryFailure>>);

impl Prepared {
    /// The summary `summary` will produce for the middle of `split` over
    /// `rows`.
    #[must_use]
    pub fn new(
        rows: &[Row],
        split: Split,
        summary: JoinHandle<Result<String, SummaryFailure>>,
    ) -> Self {
        Self {
            rows: rows
                .iter()
                .map(|r| (r.id, r.content_hash.clone()))
                .collect(),
            split,
            summary,
        }
    }

    /// Adopts the summary for `plan` when the transcript only grew since,
    /// moving `plan`'s split to the prepared middle; discards it otherwise.
    pub(crate) fn adopt(self, plan: &mut Plan) -> Option<PreparedSummary> {
        let grown = plan.rows.len() >= self.rows.len()
            && self
                .rows
                .iter()
                .zip(&plan.rows)
                .all(|((id, hash), row)| *id == row.id && *hash == row.content_hash);
        if !grown {
            tracing::debug!("the prepared summary is stale; summarizing again");
            self.summary.abort();
            return None;
        }
        plan.split = Split {
            head: self.split.head.clone(),
            middle: self.split.middle.clone(),
            tail: self.split.middle.end..plan.rows.len(),
        };
        Some(PreparedSummary(self.summary))
    }

    /// Stops writing the summary.
    pub fn discard(self) {
        self.summary.abort();
    }
}

impl PreparedSummary {
    /// The prepared summary, waiting for it if it is still being written.
    ///
    /// # Errors
    ///
    /// The summary's failure, or `Failed` when its task was stopped.
    pub async fn await_summary(self) -> Result<String, SummaryFailure> {
        self.0.await.unwrap_or_else(|err| {
            Err(SummaryFailure::new(
                FailureKind::Failed,
                format!("the prepared summary was not written: {err}"),
            ))
        })
    }
}

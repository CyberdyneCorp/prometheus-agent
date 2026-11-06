//! The seam between the turn loop and scheduled jobs (spec:
//! scheduled-jobs, Model tool surface): the binary implements
//! [`CronPort`] over the profile's job store.

use pmt_core::id::SessionId;
use pmt_tools::ToolResult;
use serde_json::Value;

use crate::turn::model::BoxFuture;

/// Scheduled jobs, as `cronjob` reaches them.
pub trait CronPort: Send + Sync {
    /// Runs one `cronjob` call of `session`.
    fn call(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult>;
}

/// What `cronjob` answers without a port.
pub const NO_CRON: &str = "cronjob is not available in this session";

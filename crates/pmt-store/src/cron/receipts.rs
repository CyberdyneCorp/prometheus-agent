//! `cron_receipts`: Bot Chat deliveries a profile admitted as turns (spec:
//! scheduled-jobs, Bot Chat delivery target). The receipt id is the
//! sending profile's outbox row id, so a retried delivery finds its
//! receipt and never admits a second turn.

use pmt_core::error::Error;
use rusqlite::{OptionalExtension, params};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

impl Store {
    /// Records `receipt` for `session_id` unless it exists; returns the
    /// session of an existing receipt (`Some`: already admitted).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn cron_admit_receipt(
        &self,
        receipt: &str,
        from_profile: &str,
        job_id: &str,
        session_id: &str,
    ) -> Result<Option<String>, Error> {
        self.write(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT session_id FROM cron_receipts WHERE receipt = ?1",
                    params![receipt],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?;
            if existing.is_some() {
                return Ok(existing);
            }
            tx.execute(
                "INSERT INTO cron_receipts (receipt, from_profile, job_id, session_id, \
                 created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![receipt, from_profile, job_id, session_id, now_ms()],
            )
            .map_err(sql)?;
            Ok(None)
        })
    }
}

//! Filtered reads of the usage ledger: the one query every usage report and
//! budget check sums from (spec: observability, Totals keyed by root
//! lineage: "for any filter, the reported total SHALL equal the sum of the
//! matching ledger rows").

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use rusqlite::types::Value;

use crate::error::sql;
use crate::store::Store;
use crate::usage::{self, UsageEvent, UsageTotals};

/// Which ledger rows a query selects; every set field narrows the
/// selection (an empty filter selects the whole profile).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFilter {
    /// Rows at or after this time.
    pub since: Option<Timestamp>,
    /// Rows strictly before this time.
    pub until: Option<Timestamp>,
    /// Rows of one root lineage (`root_session_id`).
    pub root: Option<SessionId>,
    /// Rows made by one of these sessions.
    pub sessions: Option<Vec<SessionId>>,
}

impl UsageFilter {
    /// The `WHERE` clause and its parameters.
    fn clause(&self) -> (String, Vec<Value>) {
        let mut terms = Vec::new();
        let mut params = Vec::new();
        let mut push = |term: &str, value: Value| {
            params.push(value);
            terms.push(term.replace('?', &format!("?{}", params.len())));
        };
        if let Some(since) = self.since {
            push("ts >= ?", Value::Integer(since.as_millisecond()));
        }
        if let Some(until) = self.until {
            push("ts < ?", Value::Integer(until.as_millisecond()));
        }
        if let Some(root) = self.root {
            push("root_session_id = ?", Value::Text(root.to_string()));
        }
        if let Some(sessions) = &self.sessions {
            let ids: Vec<String> = sessions.iter().map(ToString::to_string).collect();
            let list = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_owned());
            push(
                "session_id IN (SELECT value FROM json_each(?))",
                Value::Text(list),
            );
        }
        if terms.is_empty() {
            (String::new(), params)
        } else {
            (format!("WHERE {}", terms.join(" AND ")), params)
        }
    }
}

impl Store {
    /// Sums the ledger rows `filter` selects.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn usage_sum(&self, filter: &UsageFilter) -> Result<UsageTotals, Error> {
        let (clause, params) = filter.clause();
        self.read(|conn| usage::totals_with(conn, &clause, &params))
    }

    /// Calls `each` with every ledger row `filter` selects and the root
    /// session the row was recorded under, oldest first, without holding
    /// them all in memory.
    ///
    /// # Errors
    ///
    /// `store.sqlite`, or the first error `each` returns.
    pub fn for_each_usage(
        &self,
        filter: &UsageFilter,
        mut each: impl FnMut(UsageEvent, SessionId) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (clause, params) = filter.clause();
        self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "{}, root_session_id FROM usage_ledger {clause} ORDER BY ts, call_id",
                    usage::SELECT_COLUMNS
                ))
                .map_err(sql)?;
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params.iter()))
                .map_err(sql)?;
            while let Some(row) = rows.next().map_err(sql)? {
                let root = SessionId::parse(&row.get::<_, String>(20).map_err(sql)?)?;
                each(usage::event_from_row(row)?, root)?;
            }
            Ok(())
        })
    }
}

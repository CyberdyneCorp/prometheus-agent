//! Discovered model context windows (spec: model-providers, Context length
//! detection chain): the window a provider listing or a local server probe
//! reported, kept per (provider, model, base URL) so later sessions reuse
//! it while the provider is unreachable. Freshness (`catalog_ttl_secs`) is
//! the reader's decision; the store keeps the discovery time.

use jiff::Timestamp;
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, params};

use crate::error::sql;
use crate::store::Store;

/// Where a discovered window came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    /// The provider's model listing.
    Listing,
    /// A local server probe.
    Probe,
}

impl Discovery {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Listing => "listing",
            Self::Probe => "probe",
        }
    }
}

/// One discovered window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredContext {
    /// Provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Base URL without trailing slash.
    pub base_url: String,
    /// Context window in tokens.
    pub context_window: u64,
    /// How it was found.
    pub source: Discovery,
}

impl Store {
    /// Records (or refreshes) discovered windows at `now`, in one write.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn record_discovered_contexts(
        &self,
        found: &[DiscoveredContext],
        now: Timestamp,
    ) -> Result<(), Error> {
        if found.is_empty() {
            return Ok(());
        }
        self.write(|tx| {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO model_context (provider, model, base_url, context_window, source, discovered_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                     ON CONFLICT (provider, model, base_url) DO UPDATE SET \
                     context_window = excluded.context_window, source = excluded.source, \
                     discovered_at = excluded.discovered_at",
                )
                .map_err(sql)?;
            for d in found {
                let window = i64::try_from(d.context_window).unwrap_or(i64::MAX);
                stmt.execute(params![
                    d.provider,
                    d.model,
                    d.base_url.trim_end_matches('/'),
                    window,
                    d.source.as_str(),
                    now.as_millisecond()
                ])
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// The window discovered for `provider`/`model` at `base_url` and when
    /// it was discovered.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn discovered_context(
        &self,
        provider: &str,
        model: &str,
        base_url: &str,
    ) -> Result<Option<(u64, Timestamp)>, Error> {
        self.read(|conn| {
            let row = conn
                .query_row(
                    "SELECT context_window, discovered_at FROM model_context \
                     WHERE provider = ?1 AND model = ?2 AND base_url = ?3",
                    params![provider, model, base_url.trim_end_matches('/')],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(sql)?;
            Ok(row.and_then(|(window, at)| {
                Some((
                    u64::try_from(window).ok()?,
                    Timestamp::from_millisecond(at).ok()?,
                ))
            }))
        })
    }
}

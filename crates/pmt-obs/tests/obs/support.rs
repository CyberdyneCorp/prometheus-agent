//! A profile store in a temp home under the target directory, and helpers
//! to record ledger rows.

use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::SessionId;
use pmt_obs::usage::{Call, ConfigPrices, Price, Prices, UsageRecorder};
use pmt_store::{LineageKind, NewSession, Source, Store, Tokens};

pub(crate) fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

pub(crate) fn store(home: &Path) -> Arc<Store> {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    Arc::new(Store::open(&scope).unwrap())
}

pub(crate) fn root(store: &Store) -> SessionId {
    store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id
}

pub(crate) fn child(store: &Store, parent: SessionId, kind: LineageKind) -> SessionId {
    store
        .create_session(&NewSession::child(Source::Cli, parent, kind))
        .unwrap()
        .id
}

/// Fixed prices for `test/priced` (1 USD per million of every class).
pub(crate) struct TestPrices;

impl Prices for TestPrices {
    fn price(&self, provider: &str, model: &str) -> Option<Price> {
        (provider == "test" && model == "priced").then_some(Price {
            input: 1.0,
            output: 1.0,
            cache_read: 1.0,
            cache_write: 1.0,
            reasoning: None,
        })
    }
}

pub(crate) fn recorder(store: &Arc<Store>) -> UsageRecorder {
    UsageRecorder::new(Arc::clone(store), Arc::new(TestPrices))
}

pub(crate) fn config_recorder(store: &Arc<Store>, prices: ConfigPrices) -> UsageRecorder {
    UsageRecorder::new(Arc::clone(store), Arc::new(prices))
}

/// A `main` call of `session` that the provider reports as costing `usd`.
pub(crate) fn spend(rec: &UsageRecorder, session: SessionId, usd: f64, at: Timestamp) {
    let call = Call {
        reported_usd: Some(usd),
        billing_source: "api_key:test".to_owned(),
        tokens: Tokens {
            input: 100,
            output: 10,
            ..Tokens::default()
        },
        ..Call::new(session, "main", "test", "priced")
    };
    rec.record_at(call, at).unwrap();
}

/// A call of `session` using `input` + `output` tokens (unpriced model).
pub(crate) fn use_tokens(
    rec: &UsageRecorder,
    session: SessionId,
    input: u64,
    output: u64,
    at: Timestamp,
) {
    let call = Call {
        tokens: Tokens {
            input,
            output,
            ..Tokens::default()
        },
        ..Call::new(session, "main", "local", "llama")
    };
    rec.record_at(call, at).unwrap();
}

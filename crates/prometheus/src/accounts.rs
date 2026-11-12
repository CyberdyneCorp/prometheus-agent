//! A profile's credentials as every command and session resolves them
//! (spec: model-providers, Credential pool selection strategies; Exhausted
//! credential cooldowns; Borrowed CLI logins): owned keys and sign-ins,
//! the request counts and benches in `state.db`, and, when
//! `auth.adopt_external_logins` allows it, other agent CLIs' logins.

use std::path::Path;
use std::sync::Arc;

use pmt_config::{Config, ProfileScope};
use pmt_core::error::Error;
use pmt_providers::ProviderRegistry;
use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::auth::mint::Minter;
use pmt_providers::auth::{Accounts, Credentials};
use pmt_providers::detect::Detector;
use pmt_providers::quota::QuotaTracker;
use pmt_providers::spend::SpendCaps;

/// The accounts of `scope` under `config` (the profile's, or a proposed
/// one) for the user whose home is `user_home`.
///
/// # Errors
///
/// `state.db` cannot be opened.
pub fn open(
    scope: &ProfileScope,
    config: &Config,
    registry: &ProviderRegistry,
    user_home: Option<&Path>,
) -> Result<Accounts, Error> {
    let store = Arc::new(pmt_store::Store::open(scope)?);
    let secrets = scope.secrets();
    let borrowed = Borrowed::discover(config.auth.adopt_external_logins, user_home);
    let client = crate::model_cmd::client(config)?;
    Ok(Accounts::new(
        registry.clone(),
        Arc::new(secrets.clone()),
        Credentials::new(secrets.store().clone()),
    )
    .with_state(Arc::clone(&store) as _)
    .with_borrowed(borrowed)
    .with_spend_caps(SpendCaps::new(Arc::clone(&store)))
    .with_minter(Minter::new(
        client.clone(),
        user_home.map(Path::to_path_buf),
    ))
    .with_exchanges(pmt_providers::auth::exchange::Exchanges::new(
        client.clone(),
    ))
    .with_detector(Detector::new(client.clone()))
    .with_local_runtime(crate::local_cmd::runtime::for_home(
        scope.home(),
        scope.name().as_str(),
    ))
    .with_quota(QuotaTracker::new(client, store)))
}

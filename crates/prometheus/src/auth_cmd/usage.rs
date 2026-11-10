//! `prometheus auth usage [--provider ID] [--json]` (spec: model-providers,
//! Account usage windows command): the account usage of the configured or
//! given provider's sign-in, without starting a session.

use pmt_core::error::Error;
use pmt_providers::account::AccountUsage;
use pmt_providers::auth::refresh::Refresher;
use pmt_providers::auth::{Status, Subscription};
use pmt_providers::quota::{self, Probe};
use pmt_providers::registry::Provider;

use super::Profile;
use crate::context::Context;
use crate::model_cmd::{block_on, client};
use crate::output::{json, print_raw};

/// Runs the command: exit 0 with the usage, else 1 with the reason (on
/// stderr, and as `unavailable_reason` with `--json`).
pub(super) fn run(ctx: &Context<'_>, given: Option<&str>) -> Result<u8, Error> {
    let profile = Profile::open(ctx)?;
    let configured = profile.scope.config().model.provider.clone();
    let Some(name) = given.map(str::to_owned).or(configured) else {
        eprintln!("no provider configured: pass --provider <id>");
        return Ok(1);
    };
    let provider = profile.registry.lookup(&name)?;
    let usage = fetch(&profile, provider)
        .unwrap_or_else(|reason| AccountUsage::unavailable(provider.id(), &reason));
    if let Some(reason) = &usage.unavailable_reason {
        eprintln!("{}: no account usage: {reason}", provider.id());
        if ctx.json {
            json(&usage)?;
        }
        return Ok(1);
    }
    if ctx.json {
        return json(&usage);
    }
    print_raw(&format!("{}\n", usage.render()))
}

/// The usage of `provider`'s sign-in, or why there is none.
fn fetch(profile: &Profile, provider: &Provider) -> Result<AccountUsage, String> {
    let base_url = provider
        .base_url()
        .ok_or_else(|| "the provider has no endpoint".to_owned())?;
    quota::endpoint(provider.id(), base_url)
        .ok_or_else(|| "the provider has no usage endpoint".to_owned())?;
    // A sign-in token goes only to the provider's own hosts.
    let host = pmt_config::provider_urls::host_of(base_url).unwrap_or_default();
    if !provider
        .key_hosts()
        .iter()
        .any(|pattern| pmt_providers::descriptor::host_matches(pattern, &host))
    {
        return Err(format!("its sign-in may not be sent to {host}"));
    }
    let sub = signed_in(profile, provider)?;
    let refresher = Refresher::new(
        client(profile.scope.config()).map_err(|e| e.message().to_owned())?,
        Refresher::lock_dir_of(profile.scope.dir()),
    );
    // A token about to expire is refreshed first, as a session would.
    let fresh = block_on(refresher.owned(&profile.credentials, provider, &sub.label, None))
        .map_err(|e| e.message().to_owned())?
        .map_err(|_| format!("sign-in '{}' needs signing in again", sub.label))?;
    let tracker = profile
        .accounts
        .quota()
        .ok_or_else(|| "usage probes are not available".to_owned())?;
    let id = pmt_providers::pool::subscription_id(&sub.label);
    let quota = block_on(tracker.current(Probe {
        provider: provider.id(),
        credential: &id,
        base_url,
        token: &fresh.token,
        account_id: sub.account_id.as_deref(),
    }))
    .ok()
    .flatten()
    .ok_or_else(|| "the usage endpoint did not answer".to_owned())?;
    let source = format!("subscription:{}/{}", provider.id(), sub.label);
    Ok(AccountUsage::from_quota(provider.id(), &source, &quota))
}

/// The provider's first usable sign-in.
fn signed_in(profile: &Profile, provider: &Provider) -> Result<Subscription, String> {
    profile
        .credentials
        .subscriptions()
        .map_err(|e| e.message().to_owned())?
        .into_iter()
        .find(|s| s.provider == provider.id() && s.status == Status::Ok)
        .ok_or_else(|| format!("no sign-in (prometheus auth add {})", provider.id()))
}

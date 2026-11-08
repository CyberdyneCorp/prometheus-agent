//! Credential pool maintenance (spec: model-providers, Credential pool
//! maintenance commands): `auth priority` (the `fill_first` order of the
//! pool's API keys), `auth reset` (clear benches and cooldowns in
//! `state.db`, which every running session reads before each request),
//! `auth refresh` (force an OAuth refresh) and `auth logout` (remove every
//! sign-in of a provider).

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_providers::auth::refresh::Refresher;
use pmt_providers::auth::token::RefreshError;
use pmt_providers::pool::{self, PoolState};
use pmt_providers::registry::Provider;
use serde::Serialize;

use super::Profile;
use super::list::{Row, rows_of};
use super::remove::{pick, revoke};
use crate::context::Context;
use crate::model_cmd::{block_on, client};
use crate::output::{json, print_raw};

fn not_found(provider: &Provider, which: &str) -> Error {
    Error::new(
        &codes::AUTH_NOT_FOUND,
        format!("{} has no credential '{which}'", provider.id()),
    )
    .with_hint(format!("prometheus auth status {}", provider.id()))
}

fn state(profile: &Profile) -> Result<&dyn PoolState, Error> {
    profile.accounts.state().ok_or_else(|| {
        Error::new(
            &codes::AUTH_UNSUPPORTED,
            "the profile's state store is not available",
        )
    })
}

/// The rows `which` names (one), or every row when it is `None`.
fn targets<'a>(
    rows: &'a [Row],
    provider: &Provider,
    which: Option<&str>,
) -> Result<Vec<&'a Row>, Error> {
    match which {
        Some(which) => Ok(vec![
            pick(rows, which).ok_or_else(|| not_found(provider, which))?,
        ]),
        None => Ok(rows.iter().collect()),
    }
}

#[derive(Serialize)]
struct Ordered<'a> {
    provider: &'a str,
    order: Vec<&'a str>,
}

/// `auth priority <provider> <label|index> <position>`.
pub(super) fn priority(
    ctx: &Context<'_>,
    name: &str,
    which: &str,
    position: usize,
) -> Result<u8, Error> {
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let rows = rows_of(&profile, provider)?;
    let row = pick(&rows, which).ok_or_else(|| not_found(provider, which))?;
    if row.kind != "api_key" {
        return Err(Error::new(
            &codes::AUTH_UNSUPPORTED,
            format!(
                "'{}' is a sign-in; the pool order covers API keys (sign-ins follow providers.{}.prefer_credential)",
                row.label,
                provider.id()
            ),
        ));
    }
    let order = reorder(&profile, provider, &rows, &row.state_id, position)?;
    let labels: Vec<&str> = order
        .iter()
        .filter_map(|id| rows.iter().find(|r| &r.state_id == id))
        .map(|r| r.label.as_str())
        .collect();
    if ctx.json {
        return json(&Ordered {
            provider: provider.id(),
            order: labels,
        });
    }
    let mut text = format!("pool order of {}:\n", provider.id());
    for (i, label) in labels.iter().enumerate() {
        let _ = writeln!(text, "{i}. {label}");
    }
    print_raw(&text)
}

/// Moves key `id` to 0-based `position` among the pool's keys (clamped to
/// the last), renumbering the others, and stores the order.
fn reorder(
    profile: &Profile,
    provider: &Provider,
    rows: &[Row],
    id: &str,
    position: usize,
) -> Result<Vec<String>, Error> {
    let mut order: Vec<String> = rows
        .iter()
        .filter(|r| r.kind == "api_key" && r.state_id != id)
        .map(|r| r.state_id.clone())
        .collect();
    order.insert(position.min(order.len()), id.to_owned());
    pool::set_priority(state(profile)?, provider.id(), &order)?;
    Ok(order)
}

/// `auth add ... --priority <n>`: the new key `label` moves to `position`.
pub(super) fn place_key(
    profile: &Profile,
    provider: &Provider,
    label: &str,
    position: usize,
) -> Result<(), Error> {
    let rows = rows_of(profile, provider)?;
    let row = rows
        .iter()
        .find(|r| r.kind == "api_key" && r.label == label)
        .ok_or_else(|| not_found(provider, label))?;
    reorder(profile, provider, &rows, &row.state_id, position).map(drop)
}

#[derive(Serialize)]
struct Done<'a> {
    provider: &'a str,
    credentials: Vec<&'a str>,
}

/// `auth reset <provider> [<label|index>]`: benches and cooldowns cleared.
/// The pool state lives in `state.db`, read before every request, so a
/// running session (a gateway's too) uses the credential on its next
/// request.
pub(super) fn reset(ctx: &Context<'_>, name: &str, which: Option<&str>) -> Result<u8, Error> {
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let rows = rows_of(&profile, provider)?;
    let state = state(&profile)?;
    let chosen = targets(&rows, provider, which)?;
    for row in &chosen {
        state.bench(provider.id(), &row.state_id, None, None)?;
    }
    let labels: Vec<&str> = chosen.iter().map(|r| r.label.as_str()).collect();
    if ctx.json {
        return json(&Done {
            provider: provider.id(),
            credentials: labels,
        });
    }
    eprintln!(
        "cleared the benches of {} credential(s) of {}: {}",
        labels.len(),
        provider.id(),
        labels.join(", ")
    );
    Ok(0)
}

/// `auth refresh <provider> [<label|index>]`: each owned sign-in's token is
/// refreshed now (whatever its expiry) and its bench cleared.
pub(super) fn refresh(ctx: &Context<'_>, name: &str, which: Option<&str>) -> Result<u8, Error> {
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let rows = rows_of(&profile, provider)?;
    let signed_in: Vec<&Row> = targets(&rows, provider, which)?
        .into_iter()
        .filter(|r| r.subscription.is_some())
        .collect();
    if signed_in.is_empty() {
        return Err(Error::new(
            &codes::AUTH_NOT_FOUND,
            format!("{} has no sign-in of its own to refresh", provider.id()),
        )
        .with_hint(format!(
            "prometheus auth add {} --type oauth",
            provider.id()
        )));
    }
    let refresher = Refresher::new(
        client(profile.scope.config())?,
        Refresher::lock_dir_of(profile.scope.dir()),
    );
    let state = state(&profile)?;
    for row in &signed_in {
        force(&profile, &refresher, provider, row)?;
        state.bench(provider.id(), &row.state_id, None, None)?;
    }
    let labels: Vec<&str> = signed_in.iter().map(|r| r.label.as_str()).collect();
    if ctx.json {
        return json(&Done {
            provider: provider.id(),
            credentials: labels,
        });
    }
    eprintln!(
        "refreshed {} sign-in(s) of {}: {}",
        labels.len(),
        provider.id(),
        labels.join(", ")
    );
    Ok(0)
}

/// Refreshes `row`'s sign-in: its current access token is presented as
/// the refused one, which makes the refresher exchange the refresh token.
fn force(
    profile: &Profile,
    refresher: &Refresher,
    provider: &Provider,
    row: &Row,
) -> Result<(), Error> {
    let sub = row.subscription.as_ref().expect("filtered on sign-ins");
    let current = profile.credentials.access_token(sub)?;
    let outcome =
        block_on(refresher.owned(&profile.credentials, provider, &row.label, current.as_ref()))?;
    outcome.map(drop).map_err(|err| match err {
        RefreshError::Dead(why) => {
            tracing::warn!(provider = provider.id(), label = %row.label, %why, "forced refresh refused");
            pmt_providers::auth::reauth_required(provider.id(), &row.label)
        }
        RefreshError::RateLimited => Error::new(
            &codes::AUTH_REFRESH_FAILED,
            format!("{} is rate-limiting token refreshes; try again later", provider.id()),
        ),
        RefreshError::Failed(err) => err,
    })
}

/// `auth logout <provider>`: every sign-in of the provider is removed (its
/// refresh token revoked where the provider allows it).
pub(super) fn logout(ctx: &Context<'_>, name: &str) -> Result<u8, Error> {
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let mut removed = Vec::new();
    for sub in profile.credentials.subscriptions()? {
        if sub.provider != provider.id() {
            continue;
        }
        revoke(&profile, provider, &sub);
        profile.credentials.remove(&sub)?;
        removed.push(sub.label);
    }
    if ctx.json {
        return json(&Done {
            provider: provider.id(),
            credentials: removed.iter().map(String::as_str).collect(),
        });
    }
    if removed.is_empty() {
        eprintln!("{} has no sign-in to remove", provider.id());
    } else {
        eprintln!(
            "signed out of {}: removed {}",
            provider.id(),
            removed.join(", ")
        );
    }
    Ok(0)
}

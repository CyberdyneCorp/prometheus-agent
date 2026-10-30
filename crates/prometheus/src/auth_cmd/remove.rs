//! `prometheus auth remove <provider> <label|index>`: deletes the
//! credential from the secrets store; a sign-in's refresh token is first
//! revoked at the provider when it offers a revocation endpoint.

use pmt_core::error::{Error, codes};
use pmt_providers::auth::{spec, token};
use serde::Serialize;

use super::Profile;
use super::list::{Row, rows_of};
use crate::context::Context;
use crate::model_cmd::{block_on, client};
use crate::output::json;

#[derive(Serialize)]
struct Removed<'a> {
    profile: &'a str,
    provider: &'a str,
    label: &'a str,
    kind: &'static str,
    revoked: bool,
}

pub(super) fn run(ctx: &Context<'_>, name: &str, which: &str) -> Result<u8, Error> {
    let profile = Profile::open(ctx)?;
    let provider = profile.registry.lookup(name)?;
    let rows = rows_of(&profile, provider)?;
    let row = pick(&rows, which).ok_or_else(|| {
        Error::new(
            &codes::AUTH_NOT_FOUND,
            format!("{} has no credential '{which}'", provider.id()),
        )
        .with_hint(format!("prometheus auth status {}", provider.id()))
    })?;
    let revoked = if let Some(sub) = &row.subscription {
        let revoked = revoke(&profile, provider, sub);
        profile.credentials.remove(sub)?;
        revoked
    } else {
        remove_key(&profile, row)?;
        false
    };
    let removed = Removed {
        profile: profile.scope.name().as_str(),
        provider: provider.id(),
        label: &row.label,
        kind: row.kind,
        revoked,
    };
    if ctx.json {
        return json(&removed);
    }
    let note = if revoked {
        " (refresh token revoked)"
    } else {
        ""
    };
    eprintln!(
        "removed {} '{}' of {}{note}",
        row.kind,
        row.label,
        provider.id()
    );
    Ok(0)
}

/// The row named by label, else by 1-based position.
fn pick<'a>(rows: &'a [Row], which: &str) -> Option<&'a Row> {
    rows.iter().find(|r| r.label == which).or_else(|| {
        let index: usize = which.parse().ok()?;
        rows.get(index.checked_sub(1)?)
    })
}

fn remove_key(profile: &Profile, row: &Row) -> Result<(), Error> {
    let name = row.secret.as_deref().unwrap_or_default();
    if row.source == "env" {
        return Err(Error::new(
            &codes::AUTH_NOT_FOUND,
            format!("{name} comes from the launch environment, not the secrets store"),
        )
        .with_hint(format!(
            "unset {name} (or PMT_SECRET_{name}) in the environment"
        )));
    }
    profile.scope.secrets().store().remove(name)?;
    Ok(())
}

/// Revokes the sign-in's refresh token where the provider allows it; a
/// failure is reported and the local copy is deleted anyway.
fn revoke(
    profile: &Profile,
    provider: &pmt_providers::registry::Provider,
    sub: &pmt_providers::auth::Subscription,
) -> bool {
    let Ok(spec) = spec::for_provider(provider) else {
        return false;
    };
    if spec.revoke_url.is_none() {
        return false;
    }
    let attempt = || -> Result<bool, Error> {
        spec.check()?;
        let Some(refresh) = profile.credentials.refresh_token(sub)? else {
            return Ok(false);
        };
        let http = client(profile.scope.config())?;
        block_on(token::revoke(&http, &spec, &refresh))?
    };
    match attempt() {
        Ok(revoked) => revoked,
        Err(err) => {
            eprintln!(
                "warning: could not revoke the refresh token at {} ({}); the local sign-in is removed anyway",
                provider.id(),
                err.message()
            );
            false
        }
    }
}

//! `/provider reload [--now]` (spec: model-providers, Provider registry
//! changes apply at session boundaries): a session keeps the provider
//! registry it started with, so installing, removing or editing a provider
//! plugin or a `[providers.*]` block reaches new sessions only; `--now`
//! rebuilds this session's registry, moves its route onto it and records
//! `provider.registry_reloaded`.

use pmt_core::error::{Error, codes};
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::RegistryReloaded;

use super::Ctx;

/// What `/provider reload` without `--now` says.
const NEXT_SESSION: &str = "provider registry changes apply to new sessions; run /provider reload --now to apply them to this session";

pub(super) async fn provider(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        ["reload"] => Ok(NEXT_SESSION.to_owned()),
        ["reload", "--now"] | ["--now", "reload"] => reload_now(ctx).await,
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            "usage: /provider reload [--now]",
        )),
    }
}

async fn reload_now(ctx: &Ctx<'_>) -> Result<String, Error> {
    let live = ctx.live;
    let (models, changed) = ctx.factory.models.reloaded(ctx.scope, &live.plugins)?;
    let target = live.model.target();
    let port = models.port(&target).await?;
    live.model.switch(target, port);
    live.emit(Event::ProviderRegistryReloaded(RegistryReloaded {
        changed: changed.clone(),
    }))
    .await;
    Ok(if changed.is_empty() {
        "provider registry reloaded; nothing changed".to_owned()
    } else {
        format!(
            "provider registry reloaded for this session; changed: {}",
            changed.join(", ")
        )
    })
}

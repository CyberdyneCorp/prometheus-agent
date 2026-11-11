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

pub(super) async fn provider(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        ["reload"] => Ok(pmt_i18n::t!(ctx.strings(), "slash.provider.next_session")),
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
    let strings = ctx.strings();
    Ok(if changed.is_empty() {
        pmt_i18n::t!(strings, "slash.provider.unchanged")
    } else {
        let list = changed.join(", ");
        pmt_i18n::t!(strings, "slash.provider.changed", list = list)
    })
}

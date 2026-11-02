//! The discovery and presentation methods (spec: tui, Discovery RPCs
//! preserve the prompt cache; Composer and path completion; Profile scope
//! on every request): `commands.catalog`, `complete.slash`,
//! `complete.path`, `model.options`, `subagent.list`, `config.get` and
//! `session.banner`.
//! They read; none of them rebuilds a live session's system prompt,
//! changes its toolset or reloads its memory snapshot.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, profile_for, rpc};
use crate::router::Router;
use crate::server::Call;

/// Registers the discovery methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::CommandsCatalog, _, _>(catalog, commands)?
        .register::<c::CompleteSlash, _, _>(catalog, complete_slash)?
        .register::<c::CompletePath, _, _>(catalog, complete_path)?
        .register::<c::ModelOptionsList, _, _>(catalog, model_options)?
        .register::<c::SubagentListing, _, _>(catalog, subagents)?
        .register::<c::ConfigGet, _, _>(catalog, config_get)?
        .register::<c::SessionBanner, _, _>(catalog, banner)?;
    Ok(())
}

async fn catalog_of(call: &Call, session_id: &str) -> Result<c::CommandCatalog, RpcError> {
    let profile = attached_profile(call, session_id, Need::Attached)?;
    let client = call.conn.client().unwrap_or(c::ClientKind::Cli);
    call.server
        .host()
        .commands(&profile, session_id, client)
        .await
        .map_err(|e| rpc(&e))
}

async fn commands(call: Call, params: c::SessionRef) -> Result<c::CommandCatalog, RpcError> {
    catalog_of(&call, &params.session_id).await
}

/// Commands whose name or alias starts with the typed prefix (the `/` is
/// optional), executable ones first; hidden ones are never offered.
async fn complete_slash(call: Call, params: c::CompleteParams) -> Result<c::Completions, RpcError> {
    let catalog = catalog_of(&call, &params.session_id).await?;
    let typed = params.prefix.trim_start_matches('/');
    let mut items: Vec<(bool, c::Completion)> = catalog
        .commands
        .into_iter()
        .filter(|cmd| cmd.disposition != c::Disposition::Hidden)
        .filter(|cmd| {
            cmd.name.starts_with(typed) || cmd.aliases.iter().any(|a| a.starts_with(typed))
        })
        .map(|cmd| {
            let runs = cmd.disposition == c::Disposition::Exec;
            let item = c::Completion {
                text: format!("/{}", cmd.name),
                description: Some(cmd.description),
                dir: false,
            };
            (runs, item)
        })
        .collect();
    items.sort_by(|(a_runs, a), (b_runs, b)| b_runs.cmp(a_runs).then(a.text.cmp(&b.text)));
    Ok(c::Completions {
        items: items.into_iter().map(|(_, item)| item).collect(),
    })
}

async fn complete_path(call: Call, params: c::CompleteParams) -> Result<c::Completions, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    // An unreachable backend lists nothing rather than failing the composer.
    Ok(call
        .server
        .host()
        .complete_path(&profile, params)
        .await
        .unwrap_or_default())
}

async fn model_options(call: Call, params: c::SessionRef) -> Result<c::ModelOptions, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .model_options(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn subagents(call: Call, params: c::SessionRef) -> Result<c::SubagentList, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .subagents(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

async fn config_get(call: Call, params: c::ConfigGetParams) -> Result<c::ConfigValue, RpcError> {
    let profile = profile_for(&call, params.profile.as_deref())?;
    call.server
        .host()
        .config_get(&profile, &params.key)
        .await
        .map_err(|e| rpc(&e))
}

async fn banner(call: Call, params: c::SessionRef) -> Result<c::BannerInfo, RpcError> {
    let profile = attached_profile(&call, &params.session_id, Need::Attached)?;
    call.server
        .host()
        .banner(&profile, &params.session_id)
        .await
        .map_err(|e| rpc(&e))
}

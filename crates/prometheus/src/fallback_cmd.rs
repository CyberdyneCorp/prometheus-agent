//! `prometheus fallback list|add|remove|clear` (spec: model-providers,
//! Fallback chain commands): edits `[[fallback.chain]]` of the selected
//! profile, effective for new sessions. `add` validates the route
//! (credential, tool support, minimum context window) and refuses an entry
//! the chain already holds (same canonical provider, model and base URL)
//! before anything is written.

use std::fmt::Write as _;

use pmt_config::schema::{FallbackEntry, SecretName};
use pmt_core::error::{Error, codes};
use pmt_providers::resolve::{self, Target};
use pmt_providers::{ProviderRegistry, fallback, target};
use serde::Serialize;

use crate::cli::FallbackCommand;
use crate::context::Context;
use crate::model_cmd::{Models, block_on, picker, validate};
use crate::output::{document, print, print_raw};

/// Refusal of `fallback add` without an argument and without a terminal.
pub const REQUIRES_TERMINAL: &str =
    "prometheus fallback add requires a terminal; use 'prometheus fallback add <provider>/<model>'";

/// Runs one `fallback` subcommand.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &FallbackCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        FallbackCommand::List => list(ctx),
        FallbackCommand::Add { model } => add(ctx, model.as_deref()),
        FallbackCommand::Remove { entry } => remove(ctx, entry),
        FallbackCommand::Clear => clear(ctx),
    }
}

#[derive(Serialize)]
struct Row<'a> {
    index: usize,
    provider: &'a str,
    model: &'a str,
    base_url: Option<&'a str>,
    credential: Option<&'a str>,
}

fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let chain = &scope.config().fallback.chain;
    let rows: Vec<Row<'_>> = chain
        .iter()
        .enumerate()
        .map(|(i, e)| Row {
            index: i + 1,
            provider: &e.provider,
            model: &e.model,
            base_url: e.base_url.as_deref(),
            credential: e.credential.as_ref().map(|c| c.0.as_str()),
        })
        .collect();
    if ctx.json {
        return document("fallback list", &serde_json::json!({ "chain": rows }));
    }
    if rows.is_empty() {
        return print(&"the fallback chain is empty");
    }
    let mut text = String::new();
    for row in &rows {
        let _ = writeln!(text, "{}. {}/{}", row.index, row.provider, row.model);
    }
    print_raw(&text)
}

fn add(ctx: &Context<'_>, reference: Option<&str>) -> Result<u8, Error> {
    let reference = match reference {
        Some(reference) => reference.to_owned(),
        None => match picker::choose(ctx, REQUIRES_TERMINAL)? {
            Some(chosen) => chosen,
            None => return print(&"no model chosen; the chain is unchanged"),
        },
    };
    let models = Models::open(ctx)?;
    let config = models.scope.config();
    let target = target::parse(
        &reference,
        &config.model,
        &models.registry,
        config.model.provider.as_deref(),
    )?;
    refuse_duplicate(&models, &target)?;
    let resolver = models.resolver(config, None);
    let (route, _) = block_on(validate(&resolver, &models.catalog, &target))??;
    let mut chain = config.fallback.chain.clone();
    chain.push(FallbackEntry {
        provider: route.provider.clone(),
        model: route.model.clone(),
        base_url: target.base_url.clone(),
        credential: target.credential.clone().map(SecretName),
    });
    models
        .scope
        .profile()
        .config_file()
        .set_fallback_chain(&chain)?;
    print(&format!(
        "added {} as fallback entry {}; new sessions use it",
        route.label(),
        chain.len()
    ))
}

/// `fallback.duplicate` when the chain holds an entry with the identity
/// of `target` (canonical provider, lower-cased model, base URL).
fn refuse_duplicate(models: &Models, target: &Target) -> Result<(), Error> {
    let registry = &models.registry;
    let identity = |t: &Target| -> Option<(String, String, String)> {
        let provider = registry.lookup(&t.provider).ok()?;
        let url = t
            .base_url
            .as_deref()
            .or(provider.base_url())
            .unwrap_or_default();
        Some(resolve::identity(provider.id(), t.model.as_deref()?, url))
    };
    let wanted = identity(target);
    let existing = fallback::chain(models.scope.config(), registry)?;
    let Some(position) = existing.iter().position(|t| identity(t) == wanted) else {
        return Ok(());
    };
    Err(Error::new(
        &codes::FALLBACK_DUPLICATE,
        format!(
            "{}/{} is already in the fallback chain (entry {}); the chain is unchanged",
            existing[position].provider,
            existing[position].model.as_deref().unwrap_or_default(),
            position + 1
        ),
    ))
}

fn remove(ctx: &Context<'_>, entry: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let mut chain = config.fallback.chain.clone();
    let index = match entry.parse::<usize>() {
        Ok(n) if (1..=chain.len()).contains(&n) => Some(n - 1),
        Ok(_) => None,
        Err(_) => {
            let registry = ProviderRegistry::for_profile(config, Vec::new())?;
            let target = target::parse(entry, &config.model, &registry, None)?;
            let canonical = registry.lookup(&target.provider)?.id().to_owned();
            chain.iter().position(|e| {
                registry.canonical(&e.provider) == Some(canonical.as_str())
                    && target
                        .model
                        .as_deref()
                        .is_some_and(|m| m.eq_ignore_ascii_case(&e.model))
            })
        }
    };
    let Some(index) = index else {
        return Err(Error::new(
            &codes::FALLBACK_NOT_FOUND,
            format!("no fallback entry '{entry}'"),
        )
        .with_hint("prometheus fallback list"));
    };
    let removed = chain.remove(index);
    scope.profile().config_file().set_fallback_chain(&chain)?;
    print(&format!(
        "removed {}/{} (entry {})",
        removed.provider,
        removed.model,
        index + 1
    ))
}

fn clear(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let count = scope.config().fallback.chain.len();
    scope.profile().config_file().set_fallback_chain(&[])?;
    print(&format!("cleared the fallback chain ({count} entries)"))
}

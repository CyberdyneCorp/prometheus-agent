//! `pets list` and `pets install`.

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_pets::PetMeta;
use pmt_pets::gallery::{self, Entry};
use serde_json::json;

use crate::context::Context;
use crate::pets_host::{current, patch, pets};

pub(super) fn list(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    query: &str,
    limit: usize,
    installed: bool,
) -> Result<u8, Error> {
    if installed {
        return list_installed(ctx, scope, query, limit);
    }
    let url = current(scope).gallery_url;
    let client = super::client(ctx)?;
    let entries = super::runtime()?.block_on(gallery::fetch(&client, &url))?;
    let shown = gallery::filter(&entries, query, limit);
    if ctx.json {
        return crate::output::json(&json!({"gallery": url, "pets": shown}));
    }
    if shown.is_empty() {
        return crate::output::print(&format!("no gallery pet matches '{query}'"));
    }
    let lines: Vec<String> = shown
        .iter()
        .map(|e| {
            let by = if e.author.is_empty() {
                String::new()
            } else {
                format!("  by {}", e.author)
            };
            format!("{}  {}{by}", e.slug, e.name)
        })
        .collect();
    crate::output::print(&lines.join("\n"))
}

fn list_installed(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    query: &str,
    limit: usize,
) -> Result<u8, Error> {
    let config = current(scope);
    let active = pets(scope).active(&config.slug).map(|p| p.meta.slug);
    let query = query.to_lowercase();
    let installed: Vec<PetMeta> = pets(scope)
        .installed()
        .into_iter()
        .map(|p| p.meta)
        .filter(|m| {
            query.is_empty() || m.slug.contains(&query) || m.name.to_lowercase().contains(&query)
        })
        .take(limit)
        .collect();
    if ctx.json {
        return crate::output::json(&json!({
            "pets": installed,
            "active": active,
            "enabled": config.enabled,
        }));
    }
    if installed.is_empty() {
        return crate::output::print(&"no pet is installed");
    }
    let lines: Vec<String> = installed
        .iter()
        .map(|m| {
            let mark = if active.as_deref() == Some(&m.slug) {
                "*"
            } else {
                " "
            };
            format!("{mark} {}  {}", m.slug, m.name)
        })
        .collect();
    crate::output::print(&lines.join("\n"))
}

fn not_in_gallery(slug: &str) -> Error {
    Error::new(
        &codes::PET_NOT_FOUND,
        format!("no gallery pet has the slug '{slug}'"),
    )
    .with_hint("see 'prometheus pets list <query>'")
}

pub(super) fn install(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    slug: &str,
    select: bool,
    force: bool,
) -> Result<u8, Error> {
    let store = pets(scope);
    if store.get(slug).is_some() && !force {
        return Err(Error::new(
            &codes::PET_EXISTS,
            format!("pet '{slug}' is installed already"),
        )
        .with_hint("pass --force to replace it"));
    }
    let url = current(scope).gallery_url;
    let client = super::client(ctx)?;
    let runtime = super::runtime()?;
    let entries = runtime.block_on(gallery::fetch(&client, &url))?;
    let entry: &Entry = entries
        .iter()
        .find(|e| e.slug == slug)
        .ok_or_else(|| not_in_gallery(slug))?;
    let bytes = runtime.block_on(gallery::download(&client, entry))?;
    let meta = PetMeta {
        slug: entry.slug.clone(),
        name: entry.name.clone(),
        author: entry.author.clone(),
        source: entry.atlas_url.clone(),
        sha256: String::new(),
        license: entry.license.clone(),
    };
    let pet = store.install(&meta, &bytes, force)?;
    if select {
        patch(
            scope,
            &[
                ("pets.slug", slug.to_owned()),
                ("pets.enabled", "true".to_owned()),
            ],
        )?;
    }
    if ctx.json {
        return crate::output::json(&json!({
            "installed": pet.meta,
            "path": pet.dir,
            "selected": select,
        }));
    }
    let shown = if select { " and selected" } else { "" };
    crate::output::print(&format!(
        "installed '{slug}'{shown} ({})",
        pet.dir.display()
    ))
}

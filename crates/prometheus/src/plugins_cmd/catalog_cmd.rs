//! `prometheus plugins search|browse|show` (spec: plugin-system, Plugin
//! catalog; plugins CLI; WIT world versioning and compatibility).

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_plugins::catalog::Entry;
use pmt_plugins::record::InstallRecord;
use pmt_plugins::worlds::Support;

use super::manage;
use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host;

/// `search <query>` (or `browse` without one).
pub(super) fn search(ctx: &Context<'_>, query: Option<&str>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let catalog = plugins_host::catalog::current(&scope, true);
    let mut entries: Vec<&Entry> = match query {
        Some(query) => catalog.search(query),
        None => catalog.entries.iter().collect(),
    };
    if query.is_none() {
        entries.sort_by(|a, b| (&a.tier, &a.category, &a.id).cmp(&(&b.tier, &b.category, &b.id)));
    }
    if entries.is_empty() {
        return print_raw("no catalog plugins match\n");
    }
    let mut text = String::new();
    for entry in entries {
        let _ = writeln!(
            text,
            "{}  {}  {}  {}{}",
            entry.id,
            entry.tier,
            entry.category,
            entry.description,
            if catalog.blocked(&entry.id, Some(&entry.repo)).is_some() {
                "  (removed)"
            } else {
                ""
            }
        );
    }
    print_raw(&text)
}

/// `show <id>`: the installed plugin, then its catalog entry.
pub(super) fn show(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let catalog = plugins_host::catalog::current(&scope, false);
    let entry = catalog.find(id);
    let installed = manage::target(&scope, id).ok();
    if installed.is_none() && entry.is_none() {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("no plugin '{id}' is installed or in the catalog"),
        ));
    }
    let mut text = String::new();
    if let Some(target) = &installed {
        installed_text(&mut text, target);
    }
    if let Some(entry) = entry {
        let _ = writeln!(
            text,
            "catalog: {} tier, {}, {}\n  repo {}\n  ref {}\n  capabilities {}",
            entry.tier,
            entry.category,
            entry.description,
            entry.repo,
            entry.r#ref,
            entry.capabilities_summary
        );
    }
    if let Some(removed) = catalog.blocked(id, entry.map(|e| e.repo.as_str())) {
        let _ = writeln!(text, "removed from the catalog: {}", removed.reason);
    }
    print_raw(&text)
}

fn installed_text(text: &mut String, target: &manage::Target) {
    let manifest = &target.manifest;
    let _ = writeln!(
        text,
        "{} {} ({})\n{}\nlicense {}\ndigest {}",
        manifest.id,
        manifest.version,
        manifest.kind.as_str(),
        manifest.description,
        manifest.license,
        target.identity.digest
    );
    for (world, support) in &manifest.worlds {
        let note = match support {
            Support::Current => String::new(),
            Support::PreviousMajor => {
                format!(" ({}@{} (previous major))", world.name, world.version.major)
            }
        };
        let _ = writeln!(text, "world {world}{note}");
    }
    if !manifest.requires_plugins.is_empty() {
        let _ = writeln!(text, "requires {}", manifest.requires_plugins.join(", "));
    }
    let record = target
        .dir
        .as_deref()
        .and_then(|d| InstallRecord::read(d).ok().flatten());
    match record {
        Some(record) => {
            let _ = writeln!(
                text,
                "installed from {} {} at {}{}{}",
                record.source.as_str(),
                record.location,
                record.installed_at,
                record
                    .signer
                    .as_deref()
                    .map_or_else(|| ", unsigned".to_owned(), |s| format!(", signed by {s}")),
                if record.flags.allow_removed {
                    ", --allow-removed"
                } else {
                    ""
                }
            );
        }
        None => text.push_str("no install record\n"),
    }
}

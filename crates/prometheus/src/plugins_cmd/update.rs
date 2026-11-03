//! `prometheus plugins update [<id>]|check-updates|diff-grants <id>`
//! (spec: plugin-system, Capability grants are user-approved and
//! digest-bound; plugins CLI; security-boundaries, Grants pinned to plugin
//! identity).
//!
//! A plugin has an update when its catalog entry's ref moved (catalog
//! installs) or its package file changed (package installs); Git and
//! adopted installs are pinned and move only by installing anew. An update
//! that asks for nothing beyond the current grant and keeps its signer
//! replaces the plugin and carries the grant over to the new digest. One
//! that asks for more (or changed signer) keeps the current version
//! active: the new one waits in the profile's `cache/plugins/pending/`
//! until approved (`--grant`, or a yes on a terminal), and `diff-grants`
//! shows what it asks for.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::grants::GrantsFile;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_plugins::grants::{Approval, Identity, approve, select};
use pmt_plugins::host::state::StateDir;
use pmt_plugins::install::{self, Staged};
use pmt_plugins::manifest::{Manifest, describe, label};
use pmt_plugins::package;
use pmt_plugins::record::{InstallRecord, Origin};

use super::InstallArgs;
use super::install::{Fetched, from_catalog, from_package};
use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host;

/// An installed plugin with an update.
struct Available {
    id: String,
    record: InstallRecord,
    what: String,
}

/// The installed plugins (with records) of the profile.
fn installed(scope: &ProfileScope) -> Vec<(String, InstallRecord)> {
    let plugins = scope.dir().join("plugins");
    let Ok(entries) = std::fs::read_dir(&plugins) else {
        return Vec::new();
    };
    let mut found: Vec<(String, InstallRecord)> = entries
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            let record = InstallRecord::read(&e.path()).ok().flatten()?;
            Some((id, record))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// What moved for one installed plugin, if anything.
fn moved(scope: &ProfileScope, id: &str, record: &InstallRecord) -> Option<String> {
    match record.source {
        Origin::Catalog => {
            let catalog = plugins_host::catalog::current(scope, true);
            let entry = catalog.find(id)?;
            (Some(&entry.r#ref) != record.r#ref.as_ref()).then(|| {
                format!(
                    "catalog ref {} -> {}",
                    record.r#ref.as_deref().unwrap_or("-"),
                    entry.r#ref
                )
            })
        }
        Origin::Package => {
            let bytes = std::fs::read(&record.location).ok()?;
            let digest = package::digest(&bytes);
            (Some(&digest) != record.package_digest.as_ref())
                .then(|| format!("package {} changed", record.location))
        }
        Origin::Git | Origin::Adopted => None,
    }
}

fn available(scope: &ProfileScope) -> Vec<Available> {
    installed(scope)
        .into_iter()
        .filter_map(|(id, record)| {
            let what = moved(scope, &id, &record)?;
            Some(Available { id, record, what })
        })
        .collect()
}

/// `check-updates`.
pub(super) fn check(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let found = available(&scope);
    if found.is_empty() {
        return print_raw("every plugin is up to date\n");
    }
    let mut text = String::new();
    for update in found {
        let _ = writeln!(text, "{}: {}", update.id, update.what);
    }
    print_raw(&text)
}

/// `update [<id>]`.
/// The flags of `plugins update`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Flags {
    /// `--allow-unsigned`.
    pub allow_unsigned: bool,
    /// `--force` (a `caution` threat verdict).
    pub force: bool,
}

pub(super) fn update(
    ctx: &Context<'_>,
    id: Option<&str>,
    grant: Option<&str>,
    flags: Flags,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let mut found = available(&scope);
    if let Some(id) = id {
        found.retain(|u| u.id == id);
        if found.is_empty() {
            if !scope.dir().join("plugins").join(id).is_dir() {
                return Err(Error::new(
                    &codes::PLUGINS_NOT_FOUND,
                    format!("plugin '{id}' is not installed"),
                ));
            }
            return print_raw(&format!("{id} is up to date\n"));
        }
    }
    if found.is_empty() {
        return print_raw("every plugin is up to date\n");
    }
    let mut code = 0;
    for update in found {
        let args = update_args(&update.record, flags);
        code = code.max(update_one(ctx, &scope, &update, &args, grant)?);
    }
    Ok(code)
}

/// The install options an update runs with. `--allow-removed` is a
/// standing choice the install record keeps, so the update honours it and
/// the new record carries it on. `--allow-unsigned` is not: it admitted
/// the code installed then, and a new version is new code, so an unsigned
/// update needs the flag again (the new record says whether it was used).
fn update_args(record: &InstallRecord, flags: Flags) -> InstallArgs {
    InstallArgs {
        identifier: String::new(),
        git_ref: None,
        allow_unsigned: flags.allow_unsigned,
        force: flags.force,
        allow_removed: record.flags.allow_removed,
        enable: false,
        no_enable: true,
        grant: None,
    }
}

fn update_one(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    update: &Available,
    args: &InstallArgs,
    grant: Option<&str>,
) -> Result<u8, Error> {
    let fetched = match update.record.source {
        Origin::Catalog => from_catalog(scope, &update.id, args)?,
        _ => from_package(scope, Path::new(&update.record.location), args)?,
    };
    let mut fetched = fetched;
    if update.record.source == Origin::Package {
        fetched.record.location.clone_from(&update.record.location);
    }
    super::install::check_kill_list(scope, &fetched, args)?;
    // A dangerous new version is refused here, before anything replaces
    // the installed one, which stays active.
    super::threat::check(
        scope.config().plugins.scan_on_install,
        &fetched.staged,
        args.force,
        &mut fetched.record.flags,
    )?;
    let diff = difference(scope, &update.id, &fetched.staged.manifest)?;
    let signer_changed = fetched.record.signer != update.record.signer;
    let file = GrantsFile::of(scope.profile());
    let new_identity = Identity {
        id: update.id.clone(),
        digest: fetched.staged.digest.clone(),
    };
    if diff.added.is_empty() && !signer_changed {
        approve(&file, &new_identity, &diff.kept, "cli")?;
        return replace(scope, fetched);
    }
    let approved = match grant {
        Some(list) => Some(select(&diff.requested, list)?),
        None if crate::prompt::stdin_is_tty() => {
            let question = format!(
                "{}Approve and update?",
                new_capabilities_screen(&update.id, &fetched.staged, &diff)
            );
            crate::prompt::confirm(&question)?.then(|| diff.requested.clone())
        }
        None => None,
    };
    if let Some(chosen) = approved {
        approve(&file, &new_identity, &chosen, "cli")?;
        return replace(scope, fetched);
    }
    let line = requests_line(&update.id, &fetched.staged.manifest, &diff, signer_changed);
    keep_pending(scope, fetched)?;
    eprintln!("{line}");
    let _ = ctx;
    Ok(pmt_core::exit::APPROVAL_REQUIRED)
}

/// What a new version asks for, against the current grant.
struct Difference {
    /// Everything it requests.
    requested: CapabilitySet,
    /// Requested and covered by the current grant.
    kept: CapabilitySet,
    /// Requested beyond it.
    added: Vec<pmt_core::capability::Capability>,
}

fn difference(scope: &ProfileScope, id: &str, manifest: &Manifest) -> Result<Difference, Error> {
    let requested = manifest
        .capabilities
        .requested(StateDir::new(scope.dir(), id).root())?;
    let current = super::manage::target(scope, id)?;
    let granted = match Approval::of(
        &GrantsFile::of(scope.profile()).load()?,
        &current.identity,
        &current.requested,
    ) {
        Approval::Approved { granted, .. } => granted,
        Approval::Changed | Approval::Unapproved => CapabilitySet::new(),
    };
    let (kept, added): (Vec<_>, Vec<_>) = requested
        .iter()
        .cloned()
        .partition(|capability| granted.allows(capability));
    let kept: CapabilitySet = kept.into_iter().collect();
    Ok(Difference {
        requested,
        kept,
        added,
    })
}

fn requests_line(id: &str, manifest: &Manifest, diff: &Difference, signer_changed: bool) -> String {
    let labels: Vec<String> = diff.added.iter().map(label).collect();
    let mut line = format!(
        "{id} {} requests new capabilities: {}",
        manifest.version,
        labels.join(", ")
    );
    if labels.is_empty() {
        line = format!("{id} {} is signed by a different key", manifest.version);
    } else if signer_changed {
        line.push_str(" (and is signed by a different key)");
    }
    let _ = write!(
        line,
        "; the current version stays active until you approve: prometheus plugins update {id} --grant all"
    );
    line
}

fn new_capabilities_screen(id: &str, staged: &Staged, diff: &Difference) -> String {
    let mut screen = format!(
        "'{id}' {} ({}) asks for more:\n",
        staged.manifest.version, staged.digest
    );
    for capability in &diff.added {
        let _ = writeln!(screen, "  - {}", describe(capability));
    }
    screen
}

fn replace(scope: &ProfileScope, fetched: Fetched) -> Result<u8, Error> {
    let id = fetched.staged.manifest.id.clone();
    let version = fetched.staged.manifest.version.to_string();
    install::commit(fetched.staged, scope.dir(), &fetched.record, true)?;
    let _ = std::fs::remove_dir_all(pending_dir(scope, &id));
    eprintln!("updated '{id}' to {version}; sessions started from now on use it");
    Ok(0)
}

fn pending_dir(scope: &ProfileScope, id: &str) -> PathBuf {
    scope.dir().join("cache/plugins/pending").join(id)
}

/// Keeps a staged update for `diff-grants` (the install record it would
/// get rides along).
#[allow(clippy::needless_pass_by_value)] // the staged directory is consumed by the move
fn keep_pending(scope: &ProfileScope, fetched: Fetched) -> Result<(), Error> {
    let dir = pending_dir(scope, &fetched.staged.manifest.id);
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, parent, &e))?;
    }
    std::fs::rename(&fetched.staged.dir, &dir)
        .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &dir, &e))?;
    fetched.record.write(&dir)
}

/// `diff-grants <id>`: what the pending update asks for beyond the grant.
pub(super) fn diff_grants(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let dir = pending_dir(&scope, id);
    let manifest = Manifest::read(&dir, &plugins_host::running_version()).map_err(|_| {
        Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("no update of '{id}' is waiting for approval"),
        )
    })?;
    let diff = difference(&scope, id, &manifest)?;
    let mut text = format!("{id} {} asks for:\n", manifest.version);
    for capability in diff.requested.iter() {
        let state = if diff.kept.allows(capability) {
            "granted"
        } else {
            "new    "
        };
        let _ = writeln!(
            text,
            "{state}  {}  ({})",
            label(capability),
            describe(capability)
        );
    }
    print_raw(&text)
}

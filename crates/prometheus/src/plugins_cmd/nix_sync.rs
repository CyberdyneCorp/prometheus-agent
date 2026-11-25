//! `prometheus plugins nix-sync <manifest.json>` (hidden; spec:
//! plugin-system, Python package and Nix plugin sources), run by the Nix
//! modules' activation with the plugins `programs.prometheus.plugins`
//! declares: `{"plugins": [{"id", "source"} | {"id", "pypi"}]}`.
//!
//! Each declared plugin is materialized as `plugins/nix-managed-<id>/`
//! with install record source `nix` (a changed declaration replaces it,
//! an unchanged one is left alone); every `nix-managed-*` plugin the
//! manifest no longer lists is removed. Plugins with another record are
//! never touched. The declaration is the user's consent to install, so no
//! signature is required (the record says `allow_unsigned`), but the
//! threat scan, the kill list and capability grants apply as for any
//! install. Enablement belongs to the generated config.

use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::exit;
use pmt_plugins::install;
use pmt_plugins::manifest::{HERMES_MANIFEST_FILE, MANIFEST_FILE, check_id};
use pmt_plugins::record::{Flags, InstallRecord, Origin};
use serde::Deserialize;

use super::InstallArgs;
use super::install::Fetched;
use crate::context::Context;
use crate::plugins_host;

/// The directory prefix of a Nix-managed plugin.
pub(super) const PREFIX: &str = "nix-managed-";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    plugins: Vec<Declared>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Declared {
    id: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    pypi: Option<String>,
}

/// Runs the sync.
pub(super) fn sync(ctx: &Context<'_>, manifest: &Path) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let text = std::fs::read_to_string(manifest)
        .map_err(|e| Error::io(&codes::PLUGINS_MANIFEST, manifest, &e))?;
    let declared: Manifest = serde_json::from_str(&text).map_err(|e| {
        Error::new(
            &codes::PLUGINS_MANIFEST,
            format!("{}: {e}", manifest.display()),
        )
    })?;
    let mut failed = false;
    let mut keep = Vec::new();
    for plugin in &declared.plugins {
        let id = format!("{PREFIX}{}", plugin.id);
        keep.push(id.clone());
        match install_declared(&scope, &id, plugin) {
            Ok(true) => eprintln!("installed '{id}' (declared by Nix)"),
            Ok(false) => {}
            Err(err) => {
                eprintln!("{id}: {}", err.message());
                failed = true;
            }
        }
    }
    remove_dropped(&scope, &keep);
    Ok(if failed { exit::PARTIAL } else { exit::SUCCESS })
}

fn declared_args() -> InstallArgs {
    InstallArgs {
        identifier: String::new(),
        git_ref: None,
        allow_unsigned: true,
        allow_removed: false,
        enable: false,
        no_enable: true,
        grant: None,
        force: false,
    }
}

/// Installs (or replaces) one declared plugin; `false` when the installed
/// copy already matches.
fn install_declared(scope: &ProfileScope, id: &str, plugin: &Declared) -> Result<bool, Error> {
    check_id(id)?;
    let args = declared_args();
    let (mut fetched, location) = match (&plugin.source, &plugin.pypi) {
        (Some(source), None) => (stage_source(scope, id, Path::new(source))?, source.clone()),
        (None, Some(spec)) => {
            let identifier = format!("{}{spec}", super::pypi::PREFIX);
            (
                super::pypi::fetch_as(scope, &identifier, &args, Some(id))?,
                identifier,
            )
        }
        _ => {
            return Err(Error::new(
                &codes::PLUGINS_MANIFEST,
                "a Nix-declared plugin needs exactly one of `source` or `pypi`",
            ));
        }
    };
    let dir = scope.dir().join("plugins").join(id);
    let installed = InstallRecord::read(&dir).ok().flatten();
    if let Some(record) = &installed {
        if record.source != Origin::Nix {
            return Err(Error::new(
                &codes::PLUGINS_DISCOVERY,
                format!("'{id}' is installed by other means; Nix leaves it alone"),
            ));
        }
        if record.location == location && record.digest == fetched.staged.digest {
            return Ok(false);
        }
    }
    fetched.record.source = Origin::Nix;
    fetched.record.location = location;
    fetched.record.flags.allow_unsigned = true;
    super::install::check_kill_list(scope, &fetched, &args)?;
    super::threat::check(
        scope.config().plugins.scan_on_install,
        &fetched.staged,
        false,
        &mut fetched.record.flags,
    )?;
    install::commit(
        fetched.staged,
        scope.dir(),
        &fetched.record,
        installed.is_some(),
    )?;
    Ok(true)
}

/// Copies a declared directory (out of the read-only Nix store) into a
/// staging directory, renamed to `id`.
fn stage_source(scope: &ProfileScope, id: &str, source: &Path) -> Result<Fetched, Error> {
    let dir = install::staging_dir(scope.dir())?;
    let ready = copy_tree(source, &dir).and_then(|()| rename(&dir, id));
    let staged = install::stage_dir(dir, &plugins_host::running_version(), ready)?;
    let record = InstallRecord {
        source: Origin::Nix,
        location: source.display().to_string(),
        r#ref: None,
        package_digest: None,
        digest: staged.digest.clone(),
        signer: None,
        tier: None,
        installed_at: install::now(),
        flags: Flags::default(),
        held_update: None,
    };
    Ok(Fetched {
        staged,
        record,
        unsigned: None,
        repo: None,
    })
}

/// Copies `from` into `to`, following links, as plain writable files.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::PLUGINS_CATALOG, path, &e);
    let entries = std::fs::read_dir(from).map_err(|e| io(from, e))?;
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io(from, e))?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            let bytes = std::fs::read(&path).map_err(|e| io(&path, e))?;
            std::fs::write(&target, bytes).map_err(|e| io(&target, e))?;
        }
    }
    Ok(())
}

/// Gives the staged manifest the id `id`: `id` in `plugin.toml`, the
/// top-level `name` of a `plugin.yaml`.
fn rename(dir: &Path, id: &str) -> Result<(), Error> {
    let io = |path: &Path, e: std::io::Error| Error::io(&codes::PLUGINS_MANIFEST, path, &e);
    let toml_path = dir.join(MANIFEST_FILE);
    if toml_path.is_file() {
        let text = std::fs::read_to_string(&toml_path).map_err(|e| io(&toml_path, e))?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| Error::new(&codes::PLUGINS_MANIFEST, format!("plugin.toml: {e}")))?;
        doc["id"] = toml_edit::value(id);
        return std::fs::write(&toml_path, doc.to_string()).map_err(|e| io(&toml_path, e));
    }
    let yaml_path = dir.join(HERMES_MANIFEST_FILE);
    let text = std::fs::read_to_string(&yaml_path).map_err(|e| io(&yaml_path, e))?;
    let mut renamed = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if !renamed && line.starts_with("name:") {
                renamed = true;
                format!("name: {id}")
            } else {
                line.to_owned()
            }
        })
        .collect();
    let mut out = lines.join("\n");
    if !renamed {
        out = format!("name: {id}\n{out}");
    }
    std::fs::write(&yaml_path, out + "\n").map_err(|e| io(&yaml_path, e))
}

/// Removes the `nix-managed-*` plugins with a `nix` record that `keep`
/// does not list.
fn remove_dropped(scope: &ProfileScope, keep: &[String]) {
    let Ok(entries) = std::fs::read_dir(scope.dir().join("plugins")) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(PREFIX) || keep.contains(&name) {
            continue;
        }
        let nix = InstallRecord::read(&entry.path())
            .ok()
            .flatten()
            .is_some_and(|r| r.source == Origin::Nix);
        if !nix {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => eprintln!("removed '{name}' (no longer declared by Nix)"),
            Err(e) => eprintln!("{name}: cannot remove: {e}"),
        }
    }
}

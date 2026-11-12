//! `prometheus plugins validate|adopt|capabilities|remove|doctor` (spec:
//! plugin-system, plugins CLI; WASM sandbox resource limits;
//! security-boundaries, Signed plugins).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::grants::GrantsFile;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::{Error, codes};
use pmt_plugins::grants::{Approval, revoke};
use pmt_plugins::host::Runtime;
use pmt_plugins::load::State;
use pmt_plugins::manifest::{COMPONENT_FILE, Kind, Manifest, label};
use pmt_plugins::record::{Flags, InstallRecord, Origin};

use super::manage::{self, Decision};
use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host;

/// Every problem as one `plugins.manifest` error (exit 65).
fn invalid(path: &Path, err: &Error) -> Error {
    Error::new(
        &codes::PLUGINS_MANIFEST,
        format!("{}: {}", path.display(), err.message()),
    )
}

/// Checks a plugin directory: its manifest, its capability declarations
/// and, for `wasm`, that its component compiles and exports every world
/// it declares (a data-only plugin, such as a language pack, has no
/// component).
pub(super) fn check_dir(dir: &Path) -> Result<Manifest, Error> {
    let manifest =
        Manifest::read_any(dir, &plugins_host::running_version()).map_err(|e| invalid(dir, &e))?;
    manifest
        .capabilities
        .requested(&dir.join("plugin-data"))
        .map_err(|e| invalid(dir, &e))?;
    if manifest.kind == Kind::Wasm && !manifest.is_data_only() {
        let path = dir.join(COMPONENT_FILE);
        let bytes = std::fs::read(&path)
            .map_err(|e| invalid(dir, &Error::io(&codes::PLUGINS_LOAD, &path, &e)))?;
        let runtime = Runtime::new()?;
        let component = runtime
            .compile(&manifest.id, &bytes)
            .map_err(|e| invalid(dir, &e))?;
        let exported = runtime.exported_worlds(&component);
        for (world, _) in &manifest.worlds {
            if !exported.iter().any(|w| w.name == world.name) {
                return Err(invalid(
                    dir,
                    &Error::new(
                        &codes::PLUGINS_MANIFEST,
                        format!(
                            "world '{world}' is declared but the component does not export prometheus:{}/plugin",
                            world.name
                        ),
                    ),
                ));
            }
        }
    }
    Ok(manifest)
}

/// The language pack checks of `manifest` (spec: i18n, Language pack
/// validation): its warnings, printed; its errors refuse (exit 65).
fn check_locales(dir: &Path, manifest: &Manifest) -> Result<String, Error> {
    let ids: Vec<String> = manifest
        .provides_locales
        .iter()
        .map(|l| l.id.clone())
        .collect();
    let report = pmt_i18n::validate::pack(dir, &ids);
    if !report.errors.is_empty() {
        return Err(invalid(
            dir,
            &Error::new(&codes::PLUGINS_MANIFEST, report.errors.join("; ")),
        ));
    }
    let mut text = String::new();
    for warning in &report.warnings {
        let _ = writeln!(text, "warning: {warning}");
    }
    Ok(text)
}

/// `validate <path>`.
pub(super) fn validate(path: &Path) -> Result<u8, Error> {
    let manifest = check_dir(path)?;
    let warnings = check_locales(path, &manifest)?;
    if !warnings.is_empty() {
        print_raw(&warnings)?;
    }
    if manifest.kind == Kind::Python {
        let report = super::python::audit_dir(path, &manifest)?;
        print_raw(&super::python::report_text(&manifest.id, &report))?;
        let incompatible = report.verdict == pmt_plugins::python::audit::Verdict::Incompatible;
        return Ok(u8::from(incompatible));
    }
    let worlds: Vec<String> = manifest.worlds.iter().map(|(w, _)| w.to_string()).collect();
    print_raw(&format!(
        "{} {} ({}) is valid; worlds: {}\n",
        manifest.id,
        manifest.version,
        manifest.kind.as_str(),
        if worlds.is_empty() {
            "none".to_owned()
        } else {
            worlds.join(", ")
        }
    ))
}

/// The plugin directory `dir_or_id` names in the profile's `plugins/`.
fn profile_plugin_dir(plugins: &Path, dir_or_id: &str) -> Result<PathBuf, Error> {
    let given = Path::new(dir_or_id);
    let dir = if given.is_dir() {
        given
            .canonicalize()
            .map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, given, &e))?
    } else {
        plugins.join(dir_or_id)
    };
    let root = plugins
        .canonicalize()
        .unwrap_or_else(|_| plugins.to_path_buf());
    if dir.parent() != Some(root.as_path()) || !dir.is_dir() {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!(
                "{} is not a plugin directory in {}",
                dir.display(),
                root.display()
            ),
        ));
    }
    Ok(dir)
}

/// `adopt <dir>`: validates a hand-placed directory, records it as
/// `adopted` with its digest, then the capability approval.
pub(super) fn adopt(ctx: &Context<'_>, dir_or_id: &str, grant: Option<&str>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let dir = profile_plugin_dir(&scope.dir().join("plugins"), dir_or_id)?;
    let manifest = check_dir(&dir)?;
    let id = manifest.id.clone();
    InstallRecord {
        source: Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir)?,
        signer: None,
        tier: None,
        installed_at: pmt_plugins::install::now(),
        flags: Flags::default(),
        held_update: None,
    }
    .write(&dir)?;
    eprintln!("adopted '{id}' in profile '{}'", scope.name());
    let target = manage::target(&scope, &id)?;
    match manage::decide(&scope, &target, grant)? {
        Decision::Approved => {
            eprintln!("its capabilities are approved; `prometheus plugins enable {id}` enables it");
            Ok(0)
        }
        Decision::Declined => Ok(pmt_core::exit::FAILURE),
        Decision::Unanswered => Ok(manage::approval_required(ctx.json, &id, "adopt")),
    }
}

/// `capabilities [<id>]`: declared versus granted, per plugin.
pub(super) fn capabilities(ctx: &Context<'_>, id: Option<&str>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let loaded = plugins_host::for_session(&scope, &manage::cwd()).current();
    let ids: Vec<String> = match id {
        Some(id) => vec![id.to_owned()],
        None => loaded.statuses.iter().map(|s| s.id.clone()).collect(),
    };
    let grants = GrantsFile::of(scope.profile()).load()?;
    let mut text = String::new();
    for id in ids {
        let target = manage::target(&scope, &id)?;
        let granted = match Approval::of(&grants, &target.identity, &target.requested) {
            Approval::Approved { granted, .. } => granted,
            Approval::Changed | Approval::Unapproved => CapabilitySet::new(),
        };
        let words = |set: &CapabilitySet| -> String {
            let labels: Vec<String> = set.iter().map(label).collect();
            if labels.is_empty() {
                "none".to_owned()
            } else {
                labels.join(", ")
            }
        };
        let _ = writeln!(
            text,
            "{id}\n  declared: {}\n  granted: {}",
            words(&target.requested),
            words(&granted)
        );
    }
    if text.is_empty() {
        text.push_str("no plugins found\n");
    }
    print_raw(&text)
}

/// `remove <id>`: deletes the installed directory and the plugin's grants,
/// keeps its `plugin-data/`.
pub(super) fn remove(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let plugins = scope.dir().join("plugins");
    let dir = plugins.join(id);
    if !dir.is_dir() {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!(
                "plugin '{id}' is not installed in profile '{}'",
                scope.name()
            ),
        ));
    }
    std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&codes::PLUGINS_DISCOVERY, &dir, &e))?;
    let _ = revoke(&GrantsFile::of(scope.profile()), id, None);
    manage::forget_lists(&scope, id)?;
    eprintln!(
        "removed '{id}' from profile '{}'; its data in plugin-data/{id} is kept",
        scope.name()
    );
    Ok(0)
}

/// `doctor`: one line per problem; exit 1 when a plugin cannot load.
pub(super) fn doctor(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let loaded = plugins_host::for_session(&scope, &manage::cwd()).current();
    let mut text = String::new();
    let mut failing = false;
    for status in &loaded.statuses {
        match (status.state, &status.problem) {
            (State::Error, Some(problem)) => {
                failing = true;
                let _ = writeln!(text, "fail  {}: {problem}", status.id);
            }
            (State::Unadopted, _) => {
                let _ = writeln!(
                    text,
                    "warn  {}: unadopted (never loads until `prometheus plugins adopt`)",
                    status.id
                );
            }
            _ => {}
        }
        let record = scope.dir().join("plugins").join(&status.id);
        if let Ok(Some(record)) = InstallRecord::read(&record)
            && record.unsigned()
        {
            let _ = writeln!(
                text,
                "warn  {}: unsigned ({} install)",
                status.id,
                record.source.as_str()
            );
        }
    }
    for plugin in &loaded.plugins {
        if plugin.instance.suspended() {
            failing = true;
            let _ = writeln!(
                text,
                "fail  {}: suspended after repeated traps",
                plugin.manifest.id
            );
        }
    }
    for (plugin, hook) in loaded.hooks.unfired() {
        let _ = writeln!(
            text,
            "warn  {plugin}: hook '{hook}' never fires in this build"
        );
    }
    if text.is_empty() {
        text.push_str("ok    no plugin problems\n");
    }
    print_raw(&text)?;
    Ok(u8::from(failing))
}

//! `profile export` and `profile import` (spec: configuration-and-
//! profiles, Profile export excludes credentials by default; Profile
//! lifecycle commands): the archive itself is `pmt_backup::profile_archive`;
//! this adds the profile's memory entries and projects (they live in
//! `state.db`) and,
//! when asked, its secrets sealed under a passphrase read at a hidden
//! prompt.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use pmt_backup::profile_archive::{self, ExportOptions};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

fn open_scope(profile: pmt_config::ProfileDir) -> Result<ProfileScope, Error> {
    ProfileScope::open(profile, &Overrides::default(), Launch::Other)
}

/// The passphrase, at a hidden prompt (or from stdin when it is a pipe).
fn passphrase(why: &str) -> Result<SecretString, Error> {
    let value = crate::prompt::read_secret(why)?;
    if value.expose().is_empty() {
        return Err(Error::new(&codes::SECRET_EMPTY, "the passphrase is empty"));
    }
    Ok(value)
}

/// `profile export <name> -o FILE [--with-sessions] [--with-secrets]`.
///
/// # Errors
///
/// Profile, store and archive errors.
pub(super) fn export(
    home: &Home,
    name: &str,
    output: &Path,
    with_sessions: bool,
    with_secrets: bool,
    json: bool,
) -> Result<u8, Error> {
    let manifest = export_archive(home, name, output, with_sessions, with_secrets)?;
    if json {
        return crate::output::json(&manifest);
    }
    eprintln!("{}", exported_line(&manifest, output));
    Ok(0)
}

/// What `profile export` reports.
fn exported_line(manifest: &profile_archive::ExportManifest, output: &Path) -> String {
    format!(
        "exported profile '{}' ({} members) to {}",
        manifest.profile,
        manifest.members.len(),
        output.display()
    )
}

/// Writes profile `name`'s archive to `output` (spec: configuration-and-
/// profiles, Profile export excludes credentials by default); the
/// manifest written.
///
/// # Errors
///
/// Profile, store and archive errors.
pub fn export_archive(
    home: &Home,
    name: &str,
    output: &Path,
    with_sessions: bool,
    with_secrets: bool,
) -> Result<profile_archive::ExportManifest, Error> {
    let profile = home.existing_profile(&ProfileName::parse(name)?)?;
    let scope = open_scope(profile.clone())?;
    let memory = memory_jsonl(&scope)?;
    let projects = projects_json(&scope)?;
    let sealed_secrets = if with_secrets {
        let pass = passphrase("the export passphrase")?;
        Some(pmt_backup::sealed::seal(
            &pass,
            secrets_toml(&scope)?.as_bytes(),
        )?)
    } else {
        None
    };
    let options = ExportOptions {
        with_sessions,
        memory,
        projects,
        sealed_secrets,
    };
    profile_archive::export(&profile, output, &options, crate::build_info::VERSION)
}

/// The memory entries as JSONL (`None` when there are none or no store).
fn memory_jsonl(scope: &ProfileScope) -> Result<Option<Vec<u8>>, Error> {
    if !scope.path("state.db").is_file() {
        return Ok(None);
    }
    let entries = pmt_store::Store::open(scope)?.memory_entries(None)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let mut out = String::new();
    for entry in &entries {
        let _ = writeln!(out, "{}", crate::memory_cmd::entry_json(entry));
    }
    Ok(Some(out.into_bytes()))
}

/// The projects and the active project as JSON (`None` when there are
/// none or no store) (spec: projects, Projects in backups and exports).
fn projects_json(scope: &ProfileScope) -> Result<Option<Vec<u8>>, Error> {
    if !scope.path("state.db").is_file() {
        return Ok(None);
    }
    let projects = pmt_store::Store::open(scope)?.export_projects()?;
    if projects.projects.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::to_vec(&projects).ok())
}

/// The store's secrets as `NAME = "value"` lines.
fn secrets_toml(scope: &ProfileScope) -> Result<String, Error> {
    let store = scope.secrets().store();
    let mut table = toml::Table::new();
    for name in store.names()? {
        if let Some(value) = store.get(&name)? {
            table.insert(name, toml::Value::String(value.expose().to_owned()));
        }
    }
    Ok(toml::to_string(&table).unwrap_or_default())
}

/// `profile import FILE [--as <name>]`: the archive is verified and its
/// secrets opened before the profile is created, so a damaged archive or
/// a wrong passphrase leaves the home unchanged.
///
/// # Errors
///
/// Archive, profile and store errors; `profile.archive_passphrase` (78).
pub(super) fn import(
    home: &Home,
    file: &Path,
    name: Option<&str>,
    json: bool,
) -> Result<u8, Error> {
    let (manifest, profile) = import_archive(home, file, name, true)?;
    if json {
        return crate::output::json(&manifest);
    }
    eprintln!("{}", imported_line(&manifest, file, &profile));
    Ok(0)
}

/// What `profile import` reports.
fn imported_line(manifest: &profile_archive::ExportManifest, file: &Path, profile: &str) -> String {
    format!(
        "imported profile '{}' from {} as '{profile}'",
        manifest.profile,
        file.display()
    )
}

/// Creates a profile from `file`; the archive's manifest and the new
/// profile's name. An archive carrying sealed secrets needs a passphrase
/// prompt: without `secrets_prompt` it is refused before anything changes.
///
/// # Errors
///
/// Archive, profile and store errors; `profile.archive_passphrase` (78);
/// `cli.usage` for sealed secrets without a prompt.
pub fn import_archive(
    home: &Home,
    file: &Path,
    name: Option<&str>,
    secrets_prompt: bool,
) -> Result<(profile_archive::ExportManifest, String), Error> {
    let staged = profile_archive::stage(home, file)?;
    let secrets = match staged.read(profile_archive::SECRETS_ENTRY)? {
        Some(_) if !secrets_prompt => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "this archive carries secrets, which need a passphrase prompt",
            )
            .with_hint(format!("run: prometheus profile import {}", file.display())));
        }
        Some(sealed) => open_secrets(&sealed)?,
        None => BTreeMap::new(),
    };
    let profile = staged.install(home, name)?;
    pmt_store::rebind::adopt_copy(&profile)?;
    let scope = open_scope(profile.clone())?;
    for (secret, value) in secrets {
        scope
            .secrets()
            .store()
            .set(&secret, &SecretString::new(value))?;
    }
    if let Some(memory) = staged.read(profile_archive::MEMORY_ENTRY)? {
        restore_memory(&scope, &memory)?;
    }
    if let Some(projects) = staged.read(profile_archive::PROJECTS_ENTRY)? {
        restore_projects(&scope, &projects)?;
    }
    Ok((staged.manifest.clone(), profile.name().to_string()))
}

fn open_secrets(sealed: &[u8]) -> Result<BTreeMap<String, String>, Error> {
    let pass = passphrase("the archive's passphrase")?;
    let text = String::from_utf8(pmt_backup::sealed::open(&pass, sealed)?).unwrap_or_default();
    toml::from_str(&text).map_err(|e| {
        Error::new(
            &codes::PROFILE_ARCHIVE_INVALID,
            format!("the archive's secrets do not parse: {e}"),
        )
    })
}

fn restore_memory(scope: &ProfileScope, memory: &[u8]) -> Result<(), Error> {
    let text = String::from_utf8_lossy(memory);
    let items = pmt_memory::manage::read(
        Path::new("memory.jsonl"),
        &text,
        None,
        pmt_store::MemoryTier::Core,
    )?;
    let store = pmt_store::Store::open(scope)?;
    let redactor = scope.secrets().redactor()?;
    pmt_memory::manage::import(&store, &scope.config().memory, &items, &redactor)?;
    Ok(())
}

/// Writes the archive's projects; folders missing on this machine are kept
/// (`projects show` flags them).
fn restore_projects(scope: &ProfileScope, projects: &[u8]) -> Result<(), Error> {
    let projects: pmt_store::ProjectsExport = serde_json::from_slice(projects).map_err(|e| {
        Error::new(
            &codes::PROFILE_ARCHIVE_INVALID,
            format!("the archive's projects do not parse: {e}"),
        )
    })?;
    pmt_store::Store::open(scope)?.import_projects(&projects)?;
    Ok(())
}

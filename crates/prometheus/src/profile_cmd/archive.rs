//! `profile export` and `profile import` (spec: configuration-and-
//! profiles, Profile export excludes credentials by default; Profile
//! lifecycle commands): the archive itself is `pmt_backup::profile_archive`;
//! this adds the profile's memory entries (they live in `state.db`) and,
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
    let profile = home.existing_profile(&ProfileName::parse(name)?)?;
    let scope = open_scope(profile.clone())?;
    let memory = memory_jsonl(&scope)?;
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
        sealed_secrets,
    };
    let manifest = profile_archive::export(&profile, output, &options, crate::build_info::VERSION)?;
    if json {
        return crate::output::json(&manifest);
    }
    eprintln!(
        "exported profile '{}' ({} members) to {}",
        manifest.profile,
        manifest.members.len(),
        output.display()
    );
    Ok(0)
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
    let staged = profile_archive::stage(home, file)?;
    let secrets = match staged.read(profile_archive::SECRETS_ENTRY)? {
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
    if json {
        return crate::output::json(&staged.manifest);
    }
    eprintln!(
        "imported profile '{}' from {} as '{}'",
        staged.manifest.profile,
        file.display(),
        profile.name()
    );
    Ok(0)
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

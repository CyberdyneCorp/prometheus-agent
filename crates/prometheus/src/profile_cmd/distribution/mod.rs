//! `prometheus profile install|update|info` (spec: configuration-and-
//! profiles, Profile distributions). A distribution is a git repository
//! or directory with a `distribution.toml`; installing previews the files
//! and required secrets, asks, refuses an incompatible
//! `prometheus_requires` (exit 78), asks for missing required secrets and
//! creates the profile from the known content only (see [`manifest`]).
//! `<profile>/distribution.json` records where it came from, so `update`
//! can replace the distribution-owned files, merge its cron jobs and keep
//! the user's `config.toml`.

mod apply;
mod fetch;
pub mod manifest;

use std::collections::BTreeMap;
use std::io::BufRead as _;
use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileDir, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde::{Deserialize, Serialize};

use super::args::{InstallArgs, UpdateArgs};
use crate::context::Context;
use crate::output::{document, print_raw};
use crate::prompt;
use manifest::Manifest;

/// The record of an installed distribution.
pub const RECORD: &str = "distribution.json";

/// Where a profile's distribution came from and what it owns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// The distribution's name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// The git URL or directory it was installed from.
    pub source: String,
    /// The commit installed, when the source is a repository.
    pub commit: Option<String>,
    /// RFC 3339 install time.
    pub installed_at: String,
    /// RFC 3339 time of the last install or update.
    pub updated_at: String,
    /// The paths it owns.
    pub owned: Vec<String>,
    /// The MCP servers it defined.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// Its cron jobs: distribution id to job id.
    #[serde(default)]
    pub cron_jobs: BTreeMap<String, String>,
}

/// `profile`'s distribution record, when it was installed from one.
#[must_use]
pub fn record(profile: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(profile.join(RECORD)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save(profile: &Path, record: &Record) -> Result<(), Error> {
    let path = profile.join(RECORD);
    let text = serde_json::to_vec_pretty(record).expect("a record serializes");
    pmt_core::private_fs::write_atomic(&path, &text)
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
}

/// `profile install`.
///
/// # Errors
///
/// Fetch, manifest and compatibility errors; profile creation errors.
pub fn install(home: &Home, ctx: &Context<'_>, args: &InstallArgs) -> Result<u8, Error> {
    let fetched = fetch::fetch(home.root(), &args.source)?;
    let manifest = Manifest::load(&fetched.root)?;
    manifest.check_compatible(env!("CARGO_PKG_VERSION"))?;
    let name = ProfileName::parse(args.name.as_deref().unwrap_or(&manifest.name))?;
    let exists = home.profiles()?.contains(&name);
    if exists && !args.force {
        return Err(Error::new(
            &codes::PROFILE_EXISTS,
            format!("profile '{name}' already exists"),
        )
        .with_hint("pick another --name, or pass --force to install over it"));
    }
    eprint!("{}", preview(&manifest, &fetched.root, &name));
    if let Some(code) = confirm(
        &format!("install distribution into profile '{name}'"),
        args.yes,
    )? {
        return Ok(code);
    }
    let profile = if exists {
        home.existing_profile(&name)?
    } else {
        home.create_profile(name.as_str(), None)?
    };
    let scope = open(&profile)?;
    ask_secrets(&scope, &manifest)?;
    if fetched.root.join(manifest::CONFIG).is_file() && !exists {
        replace_config(&scope, &fetched.root)?;
    }
    let now = jiff::Timestamp::now().to_string();
    let record = Record {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        source: args.source.clone(),
        commit: fetched.commit.clone(),
        installed_at: now.clone(),
        updated_at: now,
        owned: Vec::new(),
        mcp_servers: Vec::new(),
        cron_jobs: BTreeMap::new(),
    };
    let record = place(&profile, ctx, &manifest, &fetched.root, record)?;
    save(profile.path(), &record)?;
    if let Some(text) = manifest
        .description
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        && super::description(home, &name).is_none()
    {
        let text: String = text
            .trim()
            .chars()
            .take(super::describe::MAX_CHARS)
            .collect();
        super::describe::write(home, &name, &text)?;
    }
    eprintln!(
        "installed distribution {} {} into profile '{name}'",
        manifest.name, manifest.version
    );
    if args.alias {
        super::default_alias(&name, home.root(), ctx);
    }
    Ok(0)
}

/// `profile update`.
///
/// # Errors
///
/// `profile.not_distribution` (66), fetch and manifest errors.
pub fn update(home: &Home, ctx: &Context<'_>, args: &UpdateArgs) -> Result<u8, Error> {
    let name = ProfileName::parse(&args.name)?;
    let profile = home.existing_profile(&name)?;
    let old = record(profile.path()).ok_or_else(|| not_distribution(&name))?;
    let fetched = fetch::fetch(home.root(), &old.source)?;
    let manifest = Manifest::load(&fetched.root)?;
    manifest.check_compatible(env!("CARGO_PKG_VERSION"))?;
    eprint!("{}", preview(&manifest, &fetched.root, &name));
    if args.force_config {
        eprintln!("  config.toml will be replaced (--force-config)");
    }
    if let Some(code) = confirm(&format!("update profile '{name}'"), args.yes)? {
        return Ok(code);
    }
    let scope = open(&profile)?;
    ask_secrets(&scope, &manifest)?;
    if args.force_config && fetched.root.join(manifest::CONFIG).is_file() {
        replace_config(&scope, &fetched.root)?;
    }
    // Paths the old version owned and the new one no longer does go too;
    // a recorded path outside the installable content (written by an older
    // build) is profile state and is left alone.
    let new_owned = manifest.owned_paths(&fetched.root);
    for path in old
        .owned
        .iter()
        .filter(|p| !new_owned.contains(p) && manifest::installable(p))
    {
        apply::replace_path(&fetched.root, profile.path(), path)?;
    }
    let record = Record {
        version: manifest.version.clone(),
        commit: fetched.commit.clone(),
        updated_at: jiff::Timestamp::now().to_string(),
        ..old
    };
    let record = place(&profile, ctx, &manifest, &fetched.root, record)?;
    save(profile.path(), &record)?;
    eprintln!(
        "updated profile '{name}' to {} {}{}",
        manifest.name,
        manifest.version,
        record
            .commit
            .as_deref()
            .map(|c| format!(" ({})", &c[..c.len().min(12)]))
            .unwrap_or_default()
    );
    Ok(0)
}

/// Copies the owned files, MCP servers and cron jobs into `profile` and
/// returns the updated record.
fn place(
    profile: &ProfileDir,
    ctx: &Context<'_>,
    manifest: &Manifest,
    root: &Path,
    mut record: Record,
) -> Result<Record, Error> {
    let owned = manifest.owned_paths(root);
    for path in owned
        .iter()
        .filter(|p| *p != manifest::MCP && *p != manifest::CRON)
    {
        manifest::check_installable(path)?;
        apply::replace_path(root, profile.path(), path)?;
    }
    let scope = open(profile)?;
    if owned.iter().any(|p| p == manifest::MCP) || record.mcp_servers.is_empty() {
        let servers = apply::mcp_servers(root)?;
        apply::apply_mcp(&scope, &servers, &record.mcp_servers)?;
        record.mcp_servers = servers.into_keys().collect();
    }
    if owned.iter().any(|p| p == manifest::CRON) || record.cron_jobs.is_empty() {
        let jobs = apply::cron_jobs(root)?;
        record.cron_jobs = apply::apply_cron(&scope, ctx.launch, &jobs, &record.cron_jobs)?;
    }
    record.owned = owned;
    Ok(record)
}

fn open(profile: &ProfileDir) -> Result<ProfileScope, Error> {
    ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other)
}

/// Replaces the profile's `config.toml` with the distribution's
/// (validated; a credential in it is refused like any config write).
fn replace_config(scope: &ProfileScope, root: &Path) -> Result<(), Error> {
    let path = root.join(manifest::CONFIG);
    let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    scope.profile().config_file().replace(&text)
}

/// What install or update will do, for the confirmation.
fn preview(manifest: &Manifest, root: &Path, name: &ProfileName) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "distribution {} {} -> profile '{name}'\n",
        manifest.name, manifest.version
    );
    if let Some(about) = &manifest.description {
        let _ = writeln!(out, "  {about}");
    }
    for path in manifest.owned_paths(root) {
        let _ = writeln!(out, "  install {path}");
    }
    if root.join(manifest::CONFIG).is_file() {
        let _ = writeln!(out, "  initial config.toml (kept on update)");
    }
    for need in &manifest.secrets_required {
        let _ = writeln!(
            out,
            "  secret {}{}{}",
            need.name,
            if need.required { " (required)" } else { "" },
            need.description
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        );
    }
    for refused in manifest::refused_files(root) {
        let _ = writeln!(out, "  never installed: {refused}");
    }
    out
}

/// `None` to go on; `Some(code)` to stop with that exit code.
fn confirm(what: &str, yes: bool) -> Result<Option<u8>, Error> {
    if yes {
        return Ok(None);
    }
    if !prompt::stdin_is_tty() {
        eprintln!("error: {what} needs confirmation; rerun with -y");
        return Ok(Some(pmt_core::exit::APPROVAL_REQUIRED));
    }
    if prompt::confirm(&format!("{what}?"))? {
        Ok(None)
    } else {
        eprintln!("nothing changed");
        Ok(Some(1))
    }
}

/// Asks for every required secret the profile does not have yet: a hidden
/// prompt on a terminal, else one line of stdin each.
fn ask_secrets(scope: &ProfileScope, manifest: &Manifest) -> Result<(), Error> {
    let store = scope.secrets().store();
    for need in manifest.secrets_required.iter().filter(|n| n.required) {
        if scope.secrets().resolve(&need.name).is_ok() {
            continue;
        }
        let value = if prompt::stdin_is_tty() {
            prompt::read_secret(&need.name)?
        } else {
            read_line_secret(&need.name)?
        };
        store.set(&need.name, &value)?;
        eprintln!("stored {} in profile '{}'", need.name, scope.name());
    }
    Ok(())
}

fn read_line_secret(name: &str) -> Result<SecretString, Error> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| prompt::read_error(&e))?;
    let value = line.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(Error::new(
            &codes::SECRET_EMPTY,
            format!("required secret {name} was not given"),
        ));
    }
    Ok(SecretString::new(value))
}

fn not_distribution(name: &ProfileName) -> Error {
    Error::new(
        &codes::PROFILE_NOT_DISTRIBUTION,
        format!("profile '{name}' was not installed from a distribution"),
    )
}

/// `profile info`.
///
/// # Errors
///
/// `profile.not_distribution` (66).
pub fn info(home: &Home, name: &str, json: bool) -> Result<u8, Error> {
    let name = ProfileName::parse(name)?;
    let profile = home.existing_profile(&name)?;
    let record = record(profile.path()).ok_or_else(|| not_distribution(&name))?;
    if json {
        return document("profile info", &record);
    }
    print_raw(&describe(&record))
}

/// The lines `info` and `delete` show.
#[must_use]
pub fn describe(record: &Record) -> String {
    format!(
        "distribution: {} {}\norigin: {}\ncommit: {}\ninstalled: {}\nlast update: {}\n",
        record.name,
        record.version,
        record.source,
        record.commit.as_deref().unwrap_or("(not a git repository)"),
        record.installed_at,
        record.updated_at,
    )
}

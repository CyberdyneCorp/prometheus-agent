//! `prometheus profile identity move|purge` (spec: configuration-and-
//! profiles, Identity migration and purge). A profile's identity is its
//! identity file (`SOUL.md`) and its identity memory: the core-tier entries
//! about the user (`target = user`), which every session renders into its
//! prompt. Both commands apply from the next session (a running session
//! keeps its frozen prompt), and purge takes a quick snapshot first.

use pmt_config::{Home, Launch, Overrides, ProfileDir, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_store::{MemoryEntry, MemoryTarget, MemoryTier, Store};

use super::args::IdentityCommand;
use crate::prompt;

/// The identity file.
pub const IDENTITY_FILE: &str = "SOUL.md";

/// The label of the snapshot purge takes.
pub const PURGE_LABEL: &str = "pre-identity-purge";

/// Runs one `profile identity` subcommand.
///
/// # Errors
///
/// Unknown profiles, store errors, a refused confirmation (exit 4 when no
/// terminal can answer).
pub fn run(home: &Home, command: &IdentityCommand) -> Result<u8, Error> {
    match command {
        IdentityCommand::Move { from, to, yes } => move_identity(home, from, to, *yes),
        IdentityCommand::Purge { name, yes } => purge(home, name, *yes),
    }
}

fn open(dir: ProfileDir) -> Result<Store, Error> {
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other)?;
    Store::open(&scope)
}

/// Whether `entry` bears the profile's identity.
fn is_identity(entry: &MemoryEntry) -> bool {
    entry.target == MemoryTarget::User && entry.tier == MemoryTier::Core
}

fn move_identity(home: &Home, from: &str, to: &str, yes: bool) -> Result<u8, Error> {
    let (from, to) = (ProfileName::parse(from)?, ProfileName::parse(to)?);
    if from == to {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "the source and target profiles are the same",
        ));
    }
    let (source, target) = (home.existing_profile(&from)?, home.existing_profile(&to)?);
    let rows: Vec<MemoryEntry> = open(source.clone())?
        .memory_entries(Some(MemoryTarget::User))?
        .into_iter()
        .filter(is_identity)
        .collect();
    let soul = source.path().join(IDENTITY_FILE);
    let has_soul = soul.is_file();
    let what = format!(
        "copy {} and {} identity memory entr{} from '{from}' to '{to}' (replacing its {IDENTITY_FILE})",
        if has_soul {
            IDENTITY_FILE
        } else {
            "no identity file"
        },
        rows.len(),
        if rows.len() == 1 { "y" } else { "ies" }
    );
    if let Some(code) = confirmed(&what, yes)? {
        return Ok(code);
    }
    if has_soul {
        let dest = target.path().join(IDENTITY_FILE);
        let text = std::fs::read(&soul).map_err(|e| Error::io(&codes::HOME_IO, &soul, &e))?;
        pmt_core::private_fs::write_atomic(&dest, &text)
            .map_err(|e| Error::io(&codes::HOME_IO, &dest, &e))?;
    }
    if !rows.is_empty() {
        open(target)?.memory_batch(None, |batch| {
            batch.author = Some(pmt_store::Author {
                origin: pmt_store::MemoryOrigin::Import,
                session: None,
            });
            let taken: std::collections::BTreeSet<String> =
                batch.entries.iter().map(|e| e.id.clone()).collect();
            for row in &rows {
                let mut row = row.clone();
                if taken.contains(&row.id) {
                    row.id = pmt_memory::id::entry();
                }
                batch.entries.push(row);
            }
            Ok(())
        })?;
    }
    eprintln!("copied the identity of '{from}' to '{to}'; it applies from its next session");
    Ok(0)
}

fn purge(home: &Home, name: &str, yes: bool) -> Result<u8, Error> {
    let name = ProfileName::parse(name)?;
    let profile = home.existing_profile(&name)?;
    let what = format!(
        "this removes {IDENTITY_FILE} and the identity memory of profile '{name}' (a snapshot is taken first)"
    );
    if !yes {
        match prompt::confirm_typed(&what, name.as_str())? {
            None => {
                eprintln!("error: purging an identity needs confirmation; rerun with --yes");
                return Ok(pmt_core::exit::APPROVAL_REQUIRED);
            }
            Some(false) => {
                eprintln!("not purged");
                return Ok(1);
            }
            Some(true) => {}
        }
    }
    let snapshot = pmt_backup::snapshot::create(
        &profile,
        pmt_backup::snapshot::Reason::Manual,
        Some(PURGE_LABEL),
    )?;
    let soul = profile.path().join(IDENTITY_FILE);
    match std::fs::remove_file(&soul) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io(&codes::HOME_IO, &soul, &e)),
    }
    let mut removed = 0usize;
    open(profile)?.memory_batch(None, |batch| {
        let before = batch.entries.len();
        batch.entries.retain(|e| !is_identity(e));
        removed = before - batch.entries.len();
        Ok(())
    })?;
    eprintln!(
        "purged the identity of '{name}' ({removed} memory entr{}); snapshot {} holds the old one; it applies from the next session",
        if removed == 1 { "y" } else { "ies" },
        snapshot.id
    );
    Ok(0)
}

/// `None` to go on; `Some(code)` to stop with that exit code.
fn confirmed(what: &str, yes: bool) -> Result<Option<u8>, Error> {
    if yes {
        return Ok(None);
    }
    if !prompt::stdin_is_tty() {
        eprintln!("error: {what}\nthis needs confirmation; rerun with --yes");
        return Ok(Some(pmt_core::exit::APPROVAL_REQUIRED));
    }
    if prompt::confirm(&format!("{what}?"))? {
        Ok(None)
    } else {
        eprintln!("nothing changed");
        Ok(Some(1))
    }
}

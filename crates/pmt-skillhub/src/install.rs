//! The hub install pipeline (spec: skills, Hub install pipeline and trust
//! policy; Skills lockfile and tamper detection; Skill signatures and
//! provenance): fetch into a quarantine directory, scan, check the
//! signature, decide by trust level, then move the skill into the profile
//! and record it in `skills.lock`.
//!
//! | trust | allowed verdicts | `--force` |
//! |---|---|---|
//! | `builtin` (official) | every verdict | - |
//! | `trusted` (`skills.trusted_sources`) | `safe`, `caution` | overrides, never `dangerous` |
//! | `community` | `safe` | overrides, never `dangerous` |

use std::path::{Path, PathBuf};

use pmt_config::schema::SkillSignaturePolicy;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_skills::lockfile::{self, Contents, Entry, LockFile, Trust};
use pmt_skills::meta::Meta;
use pmt_skills::scan::{Report, Verdict};
use pmt_skills::{digest, frontmatter};

use crate::bundle::Bundle;
use crate::hub::Hub;
use crate::ident::Identifier;
use crate::sign::{self, SIGNATURE_FILE, Signature};
use crate::sources;

/// The refusal when `--force` meets a `dangerous` verdict.
pub const FORCE_DANGEROUS: &str = "--force does not override a dangerous verdict.";
/// The refusal of a reinstall without `--force`.
pub const ALREADY_INSTALLED: &str = "Already installed. Use --force to reinstall.";

/// How to install.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Override a non-dangerous block and replace an installed copy.
    pub force: bool,
    /// The skill name when the source gives none (`--name`).
    pub name: Option<String>,
    /// Fetch this upstream ref instead of the latest (`--locked`).
    pub at: Option<String>,
    /// The digest the fetched content must have (`--locked`).
    pub expect_digest: Option<String>,
}

/// A finished install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The skill.
    pub name: String,
    /// Its directory.
    pub path: PathBuf,
    /// The canonical identifier.
    pub identifier: String,
    /// Its trust level.
    pub trust: Trust,
    /// The scan.
    pub report: Report,
    /// The signature check.
    pub signature: Signature,
    /// Notes for the user (an unsigned skill under `warn`, a forced block).
    pub warnings: Vec<String>,
    /// The skill carries a `blueprint` block (offer it as a suggestion).
    pub blueprint: bool,
    /// An installed copy was replaced.
    pub replaced: bool,
}

fn blocked(message: impl Into<String>) -> Error {
    Error::new(&codes::SKILL_SCAN_BLOCKED, message)
}

/// The verdicts `trust` allows without `--force`.
#[must_use]
pub fn allows(trust: Trust, verdict: Verdict) -> bool {
    match trust {
        Trust::Builtin => true,
        Trust::Trusted => verdict <= Verdict::Caution,
        Trust::Community => verdict == Verdict::Safe,
    }
}

/// Decides the scan: `Ok(warning)` to go on, the refusal otherwise.
fn decide(trust: Trust, report: &Report, force: bool) -> Result<Option<String>, Error> {
    if allows(trust, report.verdict) {
        return Ok(None);
    }
    if report.verdict == Verdict::Dangerous {
        return Err(blocked(format!(
            "{FORCE_DANGEROUS} security scan: {}",
            report.summary()
        ))
        .with_hint(FORCE_DANGEROUS));
    }
    let trust_name = sources::trust_text(trust);
    if force {
        return Ok(Some(format!(
            "installed with --force although {trust_name} skills need a better verdict: {}",
            report.summary()
        )));
    }
    Err(blocked(format!(
        "blocked: {trust_name} skills install only with a {} verdict; security scan: {}",
        if trust == Trust::Trusted {
            "safe or caution"
        } else {
            "safe"
        },
        report.summary()
    ))
    .with_hint("review the findings, then repeat with --force to install anyway"))
}

/// Checks the signature against `skills.signature_policy`.
fn check_signature(hub: &Hub, bundle: &Bundle) -> Result<(Signature, Option<String>), Error> {
    if bundle.trust == Trust::Builtin {
        return Ok((Signature::Verified("prometheus-release".to_owned()), None));
    }
    let signature = sign::trusted(
        sign::verify(
            bundle.files.get(SIGNATURE_FILE).map(Vec::as_slice),
            &bundle.signed_digest(),
            &hub.roots,
        ),
        &hub.trusted_signers,
    );
    let refuse = |message: String| Err(Error::new(&codes::SKILL_SIGNATURE, message));
    match (hub.signature_policy, &signature) {
        (SkillSignaturePolicy::Off, _) | (_, Signature::Verified(_)) => Ok((signature, None)),
        (_, Signature::Invalid(why)) => refuse(format!("invalid signature: {why}")),
        (SkillSignaturePolicy::Require, Signature::UntrustedSigner(who)) => refuse(format!(
            "untrusted signer {who} of '{}': skills.signature_policy = \"require\" accepts only skills.trusted_signers",
            bundle.name
        )),
        (SkillSignaturePolicy::Warn, Signature::UntrustedSigner(who)) => {
            let warning = format!(
                "'{}' is signed by {who}, who is not in skills.trusted_signers",
                bundle.name
            );
            tracing::warn!("{warning}");
            Ok((signature, Some(warning)))
        }
        (SkillSignaturePolicy::Require, Signature::Unsigned) => refuse(format!(
            "unsigned skill '{}': skills.signature_policy = \"require\" accepts only signed skills",
            bundle.name
        )),
        (SkillSignaturePolicy::Warn, Signature::Unsigned) => {
            let warning = format!("'{}' is unsigned", bundle.name);
            tracing::warn!("{warning}");
            Ok((signature, Some(warning)))
        }
    }
}

/// Names the bundle: what the source gave (frontmatter, then URL slug),
/// else `--name`.
fn name_bundle(bundle: &mut Bundle, options: &Options) -> Result<(), Error> {
    if let Some(name) = options.name.as_ref().filter(|_| bundle.name.is_empty()) {
        if !frontmatter::valid_name(name) {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!("--name '{name}' is not a valid skill name"),
            ));
        }
        bundle.rename(name);
    }
    if bundle.name.is_empty() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "cannot tell the skill name of '{}'; give it with --name <name>",
                bundle.identifier
            ),
        ));
    }
    if !frontmatter::valid_name(&bundle.name) {
        return Err(Error::new(
            &codes::SKILL_INVALID,
            format!("'{}' is not a valid skill name", bundle.name),
        ));
    }
    let renamed = bundle.name.clone();
    bundle.rename(&renamed);
    Ok(())
}

/// Where a bundle installs, relative to `skills/`: `<category>/<name>`
/// for official skills (as the catalog lays them out), else `<name>`.
fn install_rel(bundle: &Bundle) -> String {
    match &bundle.identifier {
        Identifier::Official { category, .. } => format!("{category}/{}", bundle.name),
        _ => bundle.name.clone(),
    }
}

/// The recorded install of `name`, and whether a skill directory of that
/// name exists without one (a user or agent skill).
fn existing(hub: &Hub, name: &str, rel: &str) -> (Option<Entry>, bool) {
    let entry = match LockFile::in_dir(&hub.skills_dir()).read() {
        Contents::Entries(entries) => entries.into_iter().find(|e| e.name == name),
        Contents::Unreadable(_) => None,
    };
    let dir_exists = hub.skills_dir().join(rel).join("SKILL.md").exists();
    (entry, dir_exists)
}

/// Fetches and installs `id`.
///
/// # Errors
///
/// `skill.already_installed` (exit 1, [`ALREADY_INSTALLED`]),
/// `skill.scan_blocked` (77), `skill.signature` (77), `skill.not_found`,
/// `skill.hub_unavailable`, `skill.invalid`, `skill.io`, `cli.usage`.
pub fn install(hub: &Hub, id: &Identifier, options: &Options) -> Result<Installed, Error> {
    let mut bundle = sources::fetch(hub, id, options.at.as_deref())?;
    install_bundle(hub, &mut bundle, options)
}

/// Installs a fetched bundle (the pipeline after the fetch).
///
/// # Errors
///
/// As [`install`].
pub fn install_bundle(
    hub: &Hub,
    bundle: &mut Bundle,
    options: &Options,
) -> Result<Installed, Error> {
    name_bundle(bundle, options)?;
    let rel = install_rel(bundle);
    let (entry, dir_exists) = existing(hub, &bundle.name, &rel);
    let hub_owned = entry.as_ref().is_some_and(Entry::is_hub);
    if (hub_owned || dir_exists) && !options.force {
        return Err(Error::new(
            &codes::SKILL_ALREADY_INSTALLED,
            ALREADY_INSTALLED,
        ));
    }
    if dir_exists && !hub_owned {
        return Err(Error::new(
            &codes::SKILL_ALREADY_INSTALLED,
            format!(
                "a skill named '{}' already exists in the profile and is not a hub install",
                bundle.name
            ),
        ));
    }
    if let Some(expected) = &options.expect_digest {
        let got = bundle.digest();
        if &got != expected {
            return Err(Error::new(
                &codes::SKILL_LOCK_MISMATCH,
                format!(
                    "'{}': {} (locked {expected}, upstream {got})",
                    bundle.name,
                    lockfile::DIGEST_MISMATCH
                ),
            ));
        }
    }
    let quarantine = quarantine(hub, bundle)?;
    let outcome = screen_and_commit(hub, bundle, options, &quarantine, &rel, entry.as_ref());
    let _ = std::fs::remove_dir_all(&quarantine);
    outcome
}

fn screen_and_commit(
    hub: &Hub,
    bundle: &Bundle,
    options: &Options,
    quarantine: &Path,
    rel: &str,
    previous: Option<&Entry>,
) -> Result<Installed, Error> {
    let files = pmt_skills::layout::walk(quarantine)
        .into_iter()
        .filter_map(|rel| std::fs::read(quarantine.join(&rel)).ok().map(|b| (rel, b)))
        .collect::<Vec<_>>();
    let report = hub
        .scanner
        .scan(&files)
        .map_err(|e| blocked(format!("security scan failed: {e}")))?;
    let mut warnings = Vec::new();
    warnings.extend(decide(bundle.trust, &report, options.force)?);
    let (signature, warning) = check_signature(hub, bundle)?;
    warnings.extend(warning);
    let target = hub.skills_dir().join(rel);
    let replaced = commit(hub, quarantine, &target, &bundle.name)?;
    if let Some(old) = previous.filter(|e| e.path.as_deref().is_some_and(|p| p != rel)) {
        // The recorded path comes from skills.lock, which may be edited or
        // tampered with: only a safe path under `skills/` is removed.
        match crate::maintain::install_dir(hub, old) {
            Some(dir) => {
                let _ = std::fs::remove_dir_all(dir);
            }
            None => warnings.push(format!(
                "the previous install path {:?} of {} is not under skills/; left in place",
                old.path.as_deref().unwrap_or_default(),
                old.name
            )),
        }
    }
    LockFile::in_dir(&hub.skills_dir()).upsert(Entry {
        name: bundle.name.clone(),
        source: bundle.identifier.canonical(),
        resolved_ref: Some(bundle.resolved_ref.clone()),
        content_digest: digest::dir(&target),
        trust: Some(bundle.trust),
        signature: Some(signature.record()),
        installed_at: lockfile::now(),
        category: bundle.category.clone(),
        path: Some(rel.to_owned()),
        quarantined: None,
    })?;
    let blueprint = bundle
        .front()
        .is_some_and(|front| Meta::of(&front).blueprint.is_some());
    Ok(Installed {
        name: bundle.name.clone(),
        path: target,
        identifier: bundle.identifier.canonical(),
        trust: bundle.trust,
        report,
        signature,
        warnings,
        blueprint,
        replaced,
    })
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::SKILL_IO, path, e)
}

/// Writes the bundle into a fresh quarantine directory.
fn quarantine(hub: &Hub, bundle: &Bundle) -> Result<PathBuf, Error> {
    let dir = hub.cache_dir().join("quarantine").join(format!(
        "{}-{}",
        bundle.name,
        ulid::Ulid::generate()
    ));
    private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
    for (rel, bytes) in &bundle.files {
        let path = rel.split('/').fold(dir.clone(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
        }
        private_fs::write_atomic(&path, bytes).map_err(|e| io(&path, &e))?;
    }
    Ok(dir)
}

/// Moves `from` to `target`, keeping a replaced copy under
/// `skills/.backups/`; `true` when a copy was replaced.
fn commit(hub: &Hub, from: &Path, target: &Path, name: &str) -> Result<bool, Error> {
    let replaced = target.exists();
    if replaced {
        let backups = hub.skills_dir().join(".backups");
        private_fs::create_dir(&backups).map_err(|e| io(&backups, &e))?;
        let stamp = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%S%.3fZ")
            .to_string();
        let backup = backups.join(format!("{name}-{stamp}"));
        std::fs::rename(target, &backup).map_err(|e| io(target, &e))?;
    }
    if let Some(parent) = target.parent() {
        private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
    }
    if std::fs::rename(from, target).is_err() {
        copy_tree(from, target)?;
    }
    Ok(replaced)
}

/// Copies a directory tree (the quarantine and the profile on different
/// file systems).
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    for rel in pmt_skills::layout::walk(from) {
        let source = from.join(&rel);
        let target = rel.split('/').fold(to.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = target.parent() {
            private_fs::create_dir(parent).map_err(|e| io(parent, &e))?;
        }
        let bytes = std::fs::read(&source).map_err(|e| io(&source, &e))?;
        private_fs::write_atomic(&target, &bytes).map_err(|e| io(&target, &e))?;
    }
    Ok(())
}

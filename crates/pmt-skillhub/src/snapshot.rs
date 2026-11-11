//! Skill snapshots (spec: skills, Skill publishing and snapshots): a
//! versioned JSON document of every hub-installed skill and every tap, so
//! another profile or machine can rebuild the same set through the full
//! install pipeline.

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::hub::Hub;
use crate::ident::Identifier;
use crate::install::{self, Options};
use crate::maintain;
use crate::taps::{self, Tap};

/// The snapshot format version.
pub const VERSION: u32 = 1;

/// One hub skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    /// The skill.
    pub name: String,
    /// The adapter (`github`, `official`, `clawhub`...).
    pub source: String,
    /// What `install` takes.
    pub identifier: String,
    /// Its category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The ref it was installed at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
    /// The installed content's digest.
    pub content_digest: String,
}

/// The document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// [`VERSION`].
    pub version: u32,
    /// When it was taken.
    pub exported_at: String,
    /// The hub skills.
    pub skills: Vec<Skill>,
    /// The taps.
    pub taps: Vec<Tap>,
}

/// The snapshot of the profile.
///
/// # Errors
///
/// `skill.io`, `skill.invalid` (a malformed taps file).
pub fn export(hub: &Hub) -> Result<Snapshot, Error> {
    let skills = maintain::entries(hub)?
        .into_iter()
        .map(|e| Skill {
            source: Identifier::parse(&e.source)
                .map(|id| id.source().to_owned())
                .unwrap_or_default(),
            name: e.name,
            identifier: e.source,
            category: e.category,
            resolved_ref: e.resolved_ref,
            content_digest: e.content_digest,
        })
        .collect();
    Ok(Snapshot {
        version: VERSION,
        exported_at: pmt_skills::lockfile::now(),
        skills,
        taps: taps::read(&hub.profile_dir)?,
    })
}

/// Parses a snapshot document.
///
/// # Errors
///
/// `skill.invalid` for malformed JSON or a newer version.
pub fn parse(text: &str) -> Result<Snapshot, Error> {
    let snapshot: Snapshot = serde_json::from_str(text)
        .map_err(|e| Error::new(&codes::SKILL_INVALID, format!("not a skills snapshot: {e}")))?;
    if snapshot.version > VERSION {
        return Err(Error::new(
            &codes::SKILL_INVALID,
            format!(
                "skills snapshot version {} is newer than this build reads ({VERSION})",
                snapshot.version
            ),
        ));
    }
    Ok(snapshot)
}

/// One imported entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Imported {
    /// The skill (or `tap <repo>`).
    pub name: String,
    /// `installed`, `added`, `present` or `failed`.
    pub action: String,
    /// Why it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Adds the snapshot's taps, then installs each skill through the full
/// pipeline; a skill already installed is `present` unless `force`.
#[must_use]
pub fn import(hub: &mut Hub, snapshot: &Snapshot, force: bool) -> Vec<Imported> {
    let mut out = Vec::new();
    for tap in &snapshot.taps {
        let result = taps::add(&hub.profile_dir, &tap.repo, Some(&tap.path));
        out.push(match result {
            Ok(_) => Imported {
                name: format!("tap {}", tap.repo),
                action: "added".to_owned(),
                detail: None,
            },
            Err(e) => failed(format!("tap {}", tap.repo), &e),
        });
    }
    hub.taps = taps::read(&hub.profile_dir).unwrap_or_default();
    for skill in &snapshot.skills {
        out.push(import_skill(hub, skill, force));
    }
    out
}

fn failed(name: String, e: &Error) -> Imported {
    Imported {
        name,
        action: "failed".to_owned(),
        detail: Some(e.message().to_owned()),
    }
}

fn import_skill(hub: &Hub, skill: &Skill, force: bool) -> Imported {
    if !force && maintain::entry(hub, &skill.name).is_ok() {
        return Imported {
            name: skill.name.clone(),
            action: "present".to_owned(),
            detail: None,
        };
    }
    let result = Identifier::parse(&skill.identifier).and_then(|id| {
        install::install(
            hub,
            &id,
            &Options {
                force,
                ..Options::default()
            },
        )
    });
    match result {
        Ok(_) => Imported {
            name: skill.name.clone(),
            action: "installed".to_owned(),
            detail: None,
        },
        Err(e) => failed(skill.name.clone(), &e),
    }
}

//! `/reload-skills` and `prometheus skills reload` (spec: skills,
//! /reload-skills rescans roots).
//!
//! Skill slash commands and listings are resolved from disk at each use,
//! so a skill copied into a root is invocable at once; a reload rescans
//! every root and the bundle directory and reports what was added, removed
//! and changed since the last scan. The session's system prompt never
//! changes: the model learns of the change through the per-turn note, and
//! the next session's index reflects it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::bundles;
use crate::catalog::Skill;
use crate::digest;
use crate::session::SkillsSession;

/// What one scan saw: skill and bundle fingerprints by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scan {
    /// Visible skills.
    pub skills: BTreeMap<String, String>,
    /// Bundles by slug.
    pub bundles: BTreeMap<String, String>,
}

/// A skill's fingerprint: its `SKILL.md` content and its file list (cheap:
/// no supporting file is read).
#[must_use]
pub fn fingerprint(skill: &Skill) -> String {
    let front = serde_json::Value::Object(skill.front.clone()).to_string();
    let files = skill.dir.files().join("\n");
    digest::tree([
        ("front".to_owned(), front.as_bytes()),
        ("body".to_owned(), skill.body.as_bytes()),
        ("files".to_owned(), files.as_bytes()),
    ])
}

/// The scan of `skills` (visible in the session) and the profile's
/// bundles.
#[must_use]
pub fn scan_of(skills: &[&Skill], profile_dir: &Path) -> Scan {
    Scan {
        skills: skills
            .iter()
            .map(|s| (s.name.clone(), fingerprint(s)))
            .collect(),
        bundles: bundles::load(profile_dir)
            .bundles
            .into_values()
            .map(|b| (b.slug, b.digest))
            .collect(),
    }
}

/// The session's scan now.
#[must_use]
pub fn scan(session: &SkillsSession) -> Scan {
    let (catalog, config) = session.catalog();
    scan_of(&session.visible(&catalog, &config), session.scope().dir())
}

/// Added, removed and changed names of one kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Delta {
    /// New names.
    pub added: Vec<String>,
    /// Gone names.
    pub removed: Vec<String>,
    /// Names whose content changed.
    pub changed: Vec<String>,
}

impl Delta {
    fn between(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Self {
        let mut delta = Self::default();
        for (name, digest) in new {
            match old.get(name) {
                None => delta.added.push(name.clone()),
                Some(before) if before != digest => delta.changed.push(name.clone()),
                Some(_) => {}
            }
        }
        delta.removed = old
            .keys()
            .filter(|n| !new.contains_key(*n))
            .cloned()
            .collect();
        delta
    }

    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    fn write(&self, out: &mut String, noun: &str) {
        for (label, names) in [
            ("added", &self.added),
            ("removed", &self.removed),
            ("changed", &self.changed),
        ] {
            if !names.is_empty() {
                let _ = writeln!(out, "{noun} {label}: {}", names.join(", "));
            }
        }
    }
}

/// What a reload found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    /// Skills.
    pub skills: Delta,
    /// Bundles.
    pub bundles: Delta,
}

impl Changes {
    /// The difference from `old` to `new`.
    #[must_use]
    pub fn between(old: &Scan, new: &Scan) -> Self {
        Self {
            skills: Delta::between(&old.skills, &new.skills),
            bundles: Delta::between(&old.bundles, &new.bundles),
        }
    }

    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.bundles.is_empty()
    }

    /// The report shown to the user.
    #[must_use]
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "Skills reloaded: no changes.".to_owned();
        }
        let mut text = String::from("Skills reloaded.\n");
        self.skills.write(&mut text, "skills");
        self.bundles.write(&mut text, "bundles");
        text.trim_end().to_owned()
    }
}

/// `/reload-skills` in a session: rescans and reports the difference from
/// the session's previous scan. The prompt is left alone.
#[must_use]
pub fn reload(session: &SkillsSession) -> Changes {
    let now = scan(session);
    let before = session.replace_scan(now.clone());
    Changes::between(&before, &now)
}

/// Where `prometheus skills reload` keeps the profile's last scan.
#[must_use]
pub fn state_file(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache").join("skills-scan.json")
}

/// The scan recorded in `file` (empty when missing or unreadable).
#[must_use]
pub fn read_state(file: &Path) -> Scan {
    std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Records `scan` in `file`.
///
/// # Errors
///
/// `skill.io`.
pub fn write_state(file: &Path, scan: &Scan) -> Result<(), pmt_core::Error> {
    if let Some(dir) = file.parent() {
        private_fs::create_dir(dir).map_err(|e| crate::lockfile::io(dir, &e))?;
    }
    let bytes = serde_json::to_vec(scan).unwrap_or_default();
    private_fs::write_atomic(file, &bytes).map_err(|e| crate::lockfile::io(file, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_name_added_removed_and_changed() {
        let old = Scan {
            skills: [("a".into(), "1".into()), ("b".into(), "1".into())].into(),
            bundles: BTreeMap::new(),
        };
        let new = Scan {
            skills: [("a".into(), "2".into()), ("c".into(), "1".into())].into(),
            bundles: [("dev".into(), "1".into())].into(),
        };
        let changes = Changes::between(&old, &new);
        assert_eq!(changes.skills.added, ["c"]);
        assert_eq!(changes.skills.removed, ["b"]);
        assert_eq!(changes.skills.changed, ["a"]);
        assert_eq!(
            changes.render(),
            "Skills reloaded.\nskills added: c\nskills removed: b\nskills changed: a\nbundles added: dev"
        );
        assert_eq!(
            Changes::between(&new, &new).render(),
            "Skills reloaded: no changes."
        );
    }
}

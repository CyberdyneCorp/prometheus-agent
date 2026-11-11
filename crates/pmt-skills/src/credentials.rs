//! Skill credential file passthrough (spec: skills, Skill credential file
//! passthrough): a loaded skill declaring `credential_files` gets each
//! existing file (relative to the profile home) passed to the session's
//! container or remote backend, but only with the `skill:<name>`
//! `credential_files` grant in `grants.toml`. A missing grant or file is
//! told once per session; an undeclared or ungranted file is never
//! passed. The files never become readable through the file tools: they
//! stay under the profile home the session sandbox denies.

use std::path::PathBuf;

use pmt_config::grants::{GrantsFile, Subject};
use pmt_core::capability::Capability;

use crate::catalog::Catalog;
use crate::session::SkillsSession;

/// A file to pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passed {
    /// The skill.
    pub skill: String,
    /// The path relative to the profile home (and to the backend's).
    pub rel: String,
    /// The file on this host.
    pub source: PathBuf,
}

/// Whether `skill` holds the grant.
#[must_use]
pub fn granted(session: &SkillsSession, skill: &str) -> bool {
    GrantsFile::of(session.scope().profile())
        .load()
        .is_ok_and(|g| {
            g.capabilities(&Subject::Skill(skill.to_owned()))
                .allows(&Capability::CredentialFiles)
        })
}

/// The notices of `skill` (none when it declares no credential file):
/// the missing grant, or each missing file.
#[must_use]
pub fn notices(session: &SkillsSession, skill: &crate::Skill) -> Vec<String> {
    let declared = &skill.meta.credential_files;
    if declared.is_empty() {
        return Vec::new();
    }
    if !granted(session, &skill.name) {
        return vec![format!(
            "skill '{}' declares credential files; they are passed only with the grant skill:{} credential_files",
            skill.name, skill.name
        )];
    }
    let home = session.scope().dir();
    declared
        .iter()
        .filter(|f| !home.join(&f.path).is_file())
        .map(|f| format!("credential file missing: {}", f.path))
        .collect()
}

/// The notices of `skill` not shown yet in this session, as one note.
#[must_use]
pub fn load_note(session: &SkillsSession, skill: &crate::Skill) -> Option<String> {
    let fresh: Vec<String> = notices(session, skill)
        .into_iter()
        .filter(|n| session.first_notice(n))
        .collect();
    (!fresh.is_empty()).then(|| fresh.join("\n"))
}

/// The files to pass now: the declared, existing files of the loaded,
/// granted skills.
#[must_use]
pub fn passed(session: &SkillsSession, catalog: &Catalog) -> Vec<Passed> {
    let home = session.scope().dir();
    let mut out = Vec::new();
    for name in session.loaded_skills() {
        let Some(skill) = catalog.find(&name) else {
            continue;
        };
        if skill.meta.credential_files.is_empty() || !granted(session, &name) {
            continue;
        }
        for file in &skill.meta.credential_files {
            let source = home.join(&file.path);
            if source.is_file() {
                out.push(Passed {
                    skill: name.clone(),
                    rel: file.path.clone(),
                    source,
                });
            }
        }
    }
    out
}

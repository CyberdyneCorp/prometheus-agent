//! `<profile>/grants.toml`: the single store of permanent capability grants
//! and permanent command approvals (spec: security-boundaries, Grants are
//! stored, reviewable and revocable; tool-system, Human approval choices
//! and grants file; design D14).
//!
//! Agent-triggered decisions (an `always` answer to a grant or approval
//! prompt) are written here and nowhere else: never to `config.toml`. Every
//! change is a read-modify-write under `grants.toml.lock` followed by an
//! atomic replace of the 0600 file, so the on-disk entries are always the
//! base: an entry removed by hand is never written back by a process that
//! still remembers it.
//!
//! ```toml
//! [approvals]
//! allow = ["npm run test*"]
//!
//! [[grant]]
//! id = "grant_01J..."
//! subject = "plugin:tracker@sha256:4f2a..."
//! capability = "net.connect:api.tracker.example:443"
//! surface = "cli"
//! granted_at = "2026-10-30T12:00:00Z"
//! ```

mod live;
mod subject;

use std::io;
use std::path::{Path, PathBuf};

use pmt_core::capability::{BASELINE, Capability, CapabilitySet};
use pmt_core::error::{Error, codes};
use pmt_core::id::GrantId;
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

pub use live::{LiveGrants, SessionGrants};
pub use subject::Subject;

use crate::layout::ProfileDir;
use crate::schema::SecurityConfig;

const HEADER: &str = "\
# Prometheus grants of this profile (mode 0600): permanent capability grants
# and command approvals. Review with `prometheus grants list`, remove one with
# `prometheus grants revoke <id>`. Agent decisions are recorded here, never in
# config.toml.
";

/// One persistent capability grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// `grant_<ULID>`, the handle of `prometheus grants revoke`.
    pub id: GrantId,
    /// Who holds it.
    pub subject: Subject,
    /// What it grants.
    pub capability: Capability,
    /// The surface the human answered on (`cli`, `tui`, `desktop`...).
    pub surface: String,
    /// When it was granted.
    #[serde(serialize_with = "serialize_timestamp")]
    pub granted_at: jiff::Timestamp,
}

fn serialize_timestamp<S: serde::Serializer>(
    ts: &jiff::Timestamp,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_str(ts)
}

/// Now, to the second (the precision the file keeps).
fn now() -> jiff::Timestamp {
    let now = jiff::Timestamp::now();
    jiff::Timestamp::from_second(now.as_second()).unwrap_or(now)
}

/// The contents of `grants.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// Capability grants, oldest first.
    pub grants: Vec<Grant>,
    /// `[approvals].allow`: command patterns approved with `always`.
    pub allow: Vec<String>,
    /// `[[hook]]`: approved shell hooks and gateway hook directories.
    pub hooks: Vec<HookConsent>,
}

/// What a hook consent approves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// A `[[hooks.<event>]]` shell hook.
    Shell,
    /// A gateway hook directory `<profile_home>/hooks/<name>/`.
    Directory,
}

/// One approved hook (spec: plugin-system, Shell hooks with first-use
/// consent; Gateway hook directories): bound to the digest of its program
/// or handler, so an edit needs a new approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookConsent {
    /// Shell hook or hook directory.
    pub kind: HookKind,
    /// The event of a shell hook.
    pub event: Option<String>,
    /// The shell hook's command, or the directory name.
    pub subject: String,
    /// `sha256:<hex>` of the program or handler.
    pub digest: String,
    /// When it was approved.
    pub granted_at: jiff::Timestamp,
}

impl Grants {
    /// The capabilities persistently granted to `subject`.
    #[must_use]
    pub fn capabilities(&self, subject: &Subject) -> CapabilitySet {
        self.grants
            .iter()
            .filter(|g| &g.subject == subject)
            .map(|g| g.capability.clone())
            .collect()
    }
}

/// The profile capability baseline: `security.capabilities`, else
/// [`BASELINE`].
#[must_use]
pub fn baseline(security: &SecurityConfig) -> CapabilitySet {
    let parsed = match &security.capabilities {
        Some(list) => CapabilitySet::parse(list.iter().map(|c| c.0.as_str())),
        None => CapabilitySet::parse(BASELINE),
    };
    // Config validation already refused unknown capability strings.
    parsed.unwrap_or_default()
}

/// A profile's `grants.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantsFile {
    path: PathBuf,
}

impl GrantsFile {
    /// The `grants.toml` of `profile`.
    #[must_use]
    pub fn of(profile: &ProfileDir) -> Self {
        Self::at(profile.path().join("grants.toml"))
    }

    /// The grants file at `path`.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file; a missing or empty file holds no grants.
    ///
    /// # Errors
    ///
    /// `grants.store` (unreadable, symlink, bad TOML or field),
    /// `capability.unknown` naming an unrecognized capability string.
    pub fn load(&self) -> Result<Grants, Error> {
        let text = match private_fs::read_no_follow(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Grants::default()),
            Err(e) => return Err(self.error(&e.to_string())),
        };
        self.parse(&text)
    }

    /// Records `capabilities` for `subject`, answered on `surface`; returns
    /// the new grants (capabilities the subject already holds are skipped).
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::load`]; `grants.store` when the file cannot be written.
    pub fn add(
        &self,
        subject: &Subject,
        capabilities: &CapabilitySet,
        surface: &str,
    ) -> Result<Vec<Grant>, Error> {
        self.update(|grants| {
            let held = grants.capabilities(subject);
            let added: Vec<Grant> = capabilities
                .iter()
                .filter(|c| !held.allows(c))
                .map(|capability| Grant {
                    id: GrantId::new(),
                    subject: subject.clone(),
                    capability: capability.clone(),
                    surface: surface.to_owned(),
                    granted_at: now(),
                })
                .collect();
            grants.grants.extend(added.iter().cloned());
            added
        })
    }

    /// Removes grant `id` and returns it.
    ///
    /// # Errors
    ///
    /// `grants.not_found` (exit 66); as [`GrantsFile::add`].
    pub fn revoke(&self, id: GrantId) -> Result<Grant, Error> {
        self.update(|grants| {
            let index = grants.grants.iter().position(|g| g.id == id)?;
            Some(grants.grants.remove(index))
        })?
        .ok_or_else(|| {
            Error::new(&codes::GRANTS_NOT_FOUND, format!("no grant with id '{id}'"))
                .with_hint("list the grants with `prometheus grants list`")
        })
    }

    /// Records a hook approval, replacing an earlier one of the same hook.
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::add`].
    pub fn consent(&self, consent: HookConsent) -> Result<(), Error> {
        self.update(|grants| {
            grants.hooks.retain(|c| {
                !(c.kind == consent.kind
                    && c.event == consent.event
                    && c.subject == consent.subject)
            });
            grants.hooks.push(consent);
        })
    }

    /// Removes the hook approvals `matching` selects and returns them.
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::add`].
    pub fn revoke_hooks(
        &self,
        matching: impl Fn(&HookConsent) -> bool,
    ) -> Result<Vec<HookConsent>, Error> {
        self.update(|grants| {
            let (removed, kept) = std::mem::take(&mut grants.hooks)
                .into_iter()
                .partition(|c| matching(c));
            grants.hooks = kept;
            removed
        })
    }

    /// Appends `pattern` to `[approvals].allow` (an `always` approval),
    /// merged into the entries on disk now; `false` when already present.
    ///
    /// # Errors
    ///
    /// As [`GrantsFile::add`].
    pub fn allow_command(&self, pattern: &str) -> Result<bool, Error> {
        self.update(|grants| {
            let new = !grants.allow.iter().any(|p| p == pattern);
            if new {
                grants.allow.push(pattern.to_owned());
            }
            new
        })
    }

    fn update<T>(&self, change: impl FnOnce(&mut Grants) -> T) -> Result<T, Error> {
        let _lock = private_fs::lock_beside(&self.path).map_err(|e| self.error(&e.to_string()))?;
        let mut grants = self.load()?;
        let result = change(&mut grants);
        private_fs::write_atomic(&self.path, render(&grants).as_bytes())
            .map_err(|e| self.error(&e.to_string()))?;
        Ok(result)
    }

    fn parse(&self, text: &str) -> Result<Grants, Error> {
        let raw: RawFile = toml::from_str(text).map_err(|e| self.error(e.message()))?;
        let mut grants = Vec::with_capacity(raw.grant.len());
        for entry in raw.grant {
            grants.push(self.typed(entry)?);
        }
        let mut hooks = Vec::with_capacity(raw.hook.len());
        for entry in raw.hook {
            hooks.push(HookConsent {
                kind: entry.kind,
                event: entry.event,
                subject: entry.subject,
                digest: entry.digest,
                granted_at: entry
                    .granted_at
                    .parse()
                    .map_err(|e| self.error(&format!("granted_at '{}': {e}", entry.granted_at)))?,
            });
        }
        Ok(Grants {
            grants,
            allow: raw.approvals.allow,
            hooks,
        })
    }

    fn typed(&self, raw: RawGrant) -> Result<Grant, Error> {
        let capability = Capability::parse(&raw.capability).map_err(|e| {
            Error::new(
                &codes::CAPABILITY_UNKNOWN,
                format!("{}: {}", self.path.display(), e.message()),
            )
        })?;
        Ok(Grant {
            id: GrantId::parse(&raw.id).map_err(|e| self.error(e.message()))?,
            subject: raw
                .subject
                .parse()
                .map_err(|e: Error| self.error(e.message()))?,
            capability,
            surface: raw.surface,
            granted_at: raw
                .granted_at
                .parse()
                .map_err(|e| self.error(&format!("granted_at '{}': {e}", raw.granted_at)))?,
        })
    }

    fn error(&self, problem: &str) -> Error {
        Error::new(
            &codes::GRANTS_STORE,
            format!("{}: {problem}", self.path.display()),
        )
    }
}

/// The on-disk shape, before typing each field.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    approvals: RawApprovals,
    #[serde(default)]
    grant: Vec<RawGrant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hook: Vec<RawHook>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHook {
    kind: HookKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    subject: String,
    digest: String,
    granted_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApprovals {
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrant {
    id: String,
    subject: String,
    capability: String,
    surface: String,
    granted_at: String,
}

fn render(grants: &Grants) -> String {
    let raw = RawFile {
        approvals: RawApprovals {
            allow: grants.allow.clone(),
        },
        grant: grants
            .grants
            .iter()
            .map(|g| RawGrant {
                id: g.id.to_string(),
                subject: g.subject.to_string(),
                capability: g.capability.to_string(),
                surface: g.surface.clone(),
                granted_at: g.granted_at.to_string(),
            })
            .collect(),
        hook: grants
            .hooks
            .iter()
            .map(|h| RawHook {
                kind: h.kind,
                event: h.event.clone(),
                subject: h.subject.clone(),
                digest: h.digest.clone(),
                granted_at: h.granted_at.to_string(),
            })
            .collect(),
    };
    let body = toml::to_string(&raw).expect("grants serialize to TOML");
    format!("{HEADER}\n{body}")
}

#[cfg(test)]
mod tests;

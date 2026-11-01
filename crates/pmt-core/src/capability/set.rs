//! Capability sets and their resolution: a session is the profile baseline
//! plus explicit grants ([`CapabilitySet::union`]), a subagent is a subset of
//! its parent ([`CapabilitySet::narrow`]), and a plugin or a tool gets the
//! intersection of what it declares and what it was given
//! ([`CapabilitySet::intersect`]). Only `union` can add, and it is used only
//! with grants a human made.

use std::path::Path;

use super::Capability;
use super::path::{Access, Places, canonical};
use crate::error::{Error, codes};

/// A deduplicated, sorted set of capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    caps: Vec<Capability>,
}

impl CapabilitySet {
    /// The empty set (no permission at all).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses capability strings (config, manifest, grants file).
    ///
    /// # Errors
    ///
    /// `capability.unknown` naming the first unrecognized string.
    pub fn parse<S: AsRef<str>>(strings: impl IntoIterator<Item = S>) -> Result<Self, Error> {
        strings
            .into_iter()
            .map(|s| Capability::parse(s.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .map(Self::from_iter)
    }

    /// The members.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.caps.iter()
    }

    /// Whether the set grants nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    /// The members as capability strings (for `pmt_sandbox::Policy`).
    #[must_use]
    pub fn to_strings(&self) -> Vec<String> {
        self.caps.iter().map(ToString::to_string).collect()
    }

    /// Whether some member covers `capability`.
    #[must_use]
    pub fn allows(&self, capability: &Capability) -> bool {
        self.caps.iter().any(|held| held.covers(capability))
    }

    /// `Ok` when [`CapabilitySet::allows`] `capability`.
    ///
    /// # Errors
    ///
    /// `capability.not_granted` (exit 77): `capability not granted: <cap>`.
    pub fn require(&self, capability: &Capability) -> Result<(), Error> {
        if self.allows(capability) {
            Ok(())
        } else {
            Err(not_granted(capability))
        }
    }

    /// The members of `requested` this set does not allow.
    #[must_use]
    pub fn missing(&self, requested: &Self) -> Vec<Capability> {
        requested
            .caps
            .iter()
            .filter(|c| !self.allows(c))
            .cloned()
            .collect()
    }

    /// A child's set: exactly `requested`, which must lie within this set
    /// (spec: subagent asks for more).
    ///
    /// # Errors
    ///
    /// `capability.not_granted` naming the first capability `requested`
    /// holds beyond this set.
    pub fn narrow(&self, requested: &Self) -> Result<Self, Error> {
        match self.missing(requested).first() {
            Some(extra) => Err(not_granted(extra)),
            None => Ok(requested.clone()),
        }
    }

    /// What both sets allow: for each pair of members, the narrower one when
    /// one covers the other. Pairs that only partly overlap are dropped, so
    /// the result never allows anything either set refuses.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        self.caps
            .iter()
            .flat_map(|a| other.caps.iter().filter_map(move |b| a.meet(b)))
            .collect()
    }

    /// Both sets' members (the session's baseline plus its explicit grants).
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        self.caps.iter().chain(&other.caps).cloned().collect()
    }

    /// This set without `capability` and anything it covers (revocation).
    #[must_use]
    pub fn without(&self, capability: &Capability) -> Self {
        self.caps
            .iter()
            .filter(|held| !capability.covers(held))
            .cloned()
            .collect()
    }

    /// Whether `access` to `path` is allowed, with `path` canonicalized
    /// first (a symlink out of a granted root is checked at its target).
    #[must_use]
    pub fn allows_path(&self, access: Access, path: &Path, places: &Places) -> bool {
        let path = canonical(path);
        self.caps.iter().any(|held| match (held, access) {
            (Capability::FsWrite(glob), _) | (Capability::FsRead(glob), Access::Read) => {
                places.matches(glob, &path)
            }
            _ => false,
        })
    }

    /// `Ok` when [`CapabilitySet::allows_path`].
    ///
    /// # Errors
    ///
    /// `capability.not_granted`: `capability not granted: fs.write:<path>`
    /// with the canonical path.
    pub fn require_path(&self, access: Access, path: &Path, places: &Places) -> Result<(), Error> {
        if self.allows_path(access, path, places) {
            return Ok(());
        }
        let name = match access {
            Access::Read => "fs.read",
            Access::Write => "fs.write",
        };
        Err(Error::new(
            &codes::CAPABILITY_NOT_GRANTED,
            format!(
                "capability not granted: {name}:{}",
                canonical(path).display()
            ),
        ))
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        let mut caps: Vec<Capability> = iter.into_iter().collect();
        caps.sort();
        caps.dedup();
        Self { caps }
    }
}

impl<'a> IntoIterator for &'a CapabilitySet {
    type Item = &'a Capability;
    type IntoIter = std::slice::Iter<'a, Capability>;

    fn into_iter(self) -> Self::IntoIter {
        self.caps.iter()
    }
}

/// `capability not granted: <cap>`.
#[must_use]
pub fn not_granted(capability: &Capability) -> Error {
    Error::new(
        &codes::CAPABILITY_NOT_GRANTED,
        format!("capability not granted: {capability}"),
    )
}

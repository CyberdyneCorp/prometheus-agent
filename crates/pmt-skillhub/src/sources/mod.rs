//! The source adapters, one per identifier form, behind [`fetch`].

pub mod github;
pub mod official;
pub mod registries;
pub mod skills_sh;
pub mod web;

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_skills::lockfile::Trust;
use serde::Serialize;

use crate::bundle::Bundle;
use crate::hub::Hub;
use crate::ident::Identifier;

/// One search or browse result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    /// What `install` takes.
    pub identifier: String,
    /// The skill name.
    pub name: String,
    /// One line.
    pub description: String,
    /// The adapter.
    pub source: String,
    /// Its trust level.
    #[serde(serialize_with = "trust_name")]
    pub trust: Trust,
    /// Installs reported by the directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installs: Option<u64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's serialize_with signature
fn trust_name<S: serde::Serializer>(trust: &Trust, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(trust_text(*trust))
}

/// `builtin`, `trusted` or `community`.
#[must_use]
pub fn trust_text(trust: Trust) -> &'static str {
    match trust {
        Trust::Builtin => "builtin",
        Trust::Trusted => "trusted",
        Trust::Community => "community",
    }
}

/// Percent-encodes a query value.
#[must_use]
pub fn encode(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

/// Fetches the skill `id` names, at upstream ref `at` when given (a
/// commit, a version), without writing anything.
///
/// # Errors
///
/// `skill.not_found`, `skill.hub_unavailable`, `skill.invalid`.
pub fn fetch(hub: &Hub, id: &Identifier, at: Option<&str>) -> Result<Bundle, Error> {
    match id {
        Identifier::Official { category, name } => official::fetch(hub, category, name),
        Identifier::SkillsSh { owner, repo, skill } => {
            skills_sh::fetch(hub, owner, repo, skill, at)
        }
        Identifier::WellKnown { url } => web::fetch_well_known(hub, url),
        Identifier::GitHub { owner, repo, path } => {
            github::fetch(hub, id.clone(), &format!("{owner}/{repo}"), path, at)
        }
        Identifier::ClawHub { slug } => registries::fetch_clawhub(hub, slug, at),
        Identifier::LobeHub { slug } => registries::fetch_lobehub(hub, slug),
        Identifier::BrowseSh { slug } => registries::fetch_browse_sh(hub, slug),
        Identifier::Url { url } => web::fetch_url(hub, url),
    }
}

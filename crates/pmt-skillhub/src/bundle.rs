//! A fetched skill: its files in memory plus where they came from. Nothing
//! is written until the install pipeline accepts it.

use std::collections::BTreeMap;

use pmt_core::error::{Error, codes};
use pmt_skills::layout::{self, SKILL_FILE};
use pmt_skills::lockfile::Trust;
use pmt_skills::{digest, frontmatter};
use serde::Serialize;

use crate::ident::Identifier;
use crate::sign::SIGNATURE_FILE;

/// What the upstream says about a skill (shown by `inspect`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Upstream {
    /// The repository or page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    /// Installs the directory reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installs: Option<u64>,
    /// The directory's own audit status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<String>,
}

/// A fetched skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// The skill name (empty: the source could not tell; `--name` must).
    pub name: String,
    /// Relative path to content.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Where it came from.
    pub identifier: Identifier,
    /// The ref the fetch resolved to (commit, version or URL).
    pub resolved_ref: String,
    /// Its source's trust level.
    pub trust: Trust,
    /// Its category, when the source or frontmatter tells.
    pub category: Option<String>,
    /// Upstream metadata.
    pub upstream: Upstream,
}

/// Whether `rel` may be written below a skill directory: relative,
/// `/`-separated, no `.`/`..` segment, no excluded directory.
#[must_use]
pub fn safe_path(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.contains(':')
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !layout::excluded(part))
}

impl Bundle {
    /// A bundle of `files`, refusing unsafe paths and a missing `SKILL.md`.
    ///
    /// # Errors
    ///
    /// `skill.invalid`.
    pub fn new(
        identifier: Identifier,
        files: BTreeMap<String, Vec<u8>>,
        resolved_ref: String,
        trust: Trust,
    ) -> Result<Self, Error> {
        if let Some(bad) = files.keys().find(|p| !safe_path(p)) {
            return Err(Error::new(
                &codes::SKILL_INVALID,
                format!("{identifier}: unsafe file path '{bad}' in the skill"),
            ));
        }
        if !files.contains_key(SKILL_FILE) {
            return Err(Error::new(
                &codes::SKILL_INVALID,
                format!("{identifier}: no {SKILL_FILE}"),
            ));
        }
        let mut bundle = Self {
            name: String::new(),
            files,
            identifier,
            resolved_ref,
            trust,
            category: None,
            upstream: Upstream::default(),
        };
        let front = bundle.front();
        front
            .as_ref()
            .and_then(|f| f.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|n| frontmatter::valid_name(n))
            .unwrap_or_default()
            .clone_into(&mut bundle.name);
        bundle.category = front
            .as_ref()
            .and_then(|f| frontmatter::field(f, "category"))
            .and_then(serde_json::Value::as_str)
            .filter(|c| frontmatter::valid_name(c))
            .map(str::to_owned);
        Ok(bundle)
    }

    /// The `SKILL.md` text.
    #[must_use]
    pub fn skill_md(&self) -> String {
        self.files
            .get(SKILL_FILE)
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    }

    /// The frontmatter mapping.
    #[must_use]
    pub fn front(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        frontmatter::mapping(&self.skill_md())
    }

    /// The frontmatter description.
    #[must_use]
    pub fn description(&self) -> String {
        self.front()
            .and_then(|f| {
                f.get("description")
                    .and_then(|d| d.as_str().map(str::to_owned))
            })
            .unwrap_or_default()
    }

    /// The digest of the files as installed ([`digest::tree`]).
    #[must_use]
    pub fn digest(&self) -> String {
        digest::tree(self.files.iter().map(|(p, b)| (p.clone(), b.as_slice())))
    }

    /// The digest a signature covers: every file but `SKILL.sig`.
    #[must_use]
    pub fn signed_digest(&self) -> String {
        digest::tree(
            self.files
                .iter()
                .filter(|(p, _)| p.as_str() != SIGNATURE_FILE)
                .map(|(p, b)| (p.clone(), b.as_slice())),
        )
    }

    /// The files, for a scan.
    #[must_use]
    pub fn scan_input(&self) -> Vec<(String, Vec<u8>)> {
        self.files
            .iter()
            .map(|(p, b)| (p.clone(), b.clone()))
            .collect()
    }

    /// Names the skill `name`, writing `name:` into a frontmatter that
    /// has none (a direct URL install named with `--name`).
    pub fn rename(&mut self, name: &str) {
        let text = self.skill_md();
        let has_name = frontmatter::mapping(&text).is_some_and(|f| f.contains_key("name"));
        if !has_name {
            let patched = match text.strip_prefix("---\n") {
                Some(rest) => format!("---\nname: {name}\n{rest}"),
                None => format!("---\nname: {name}\ndescription: {name}\n---\n{text}"),
            };
            self.files
                .insert(SKILL_FILE.to_owned(), patched.into_bytes());
        }
        name.clone_into(&mut self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_paths_are_refused() {
        for bad in [
            "../x",
            "/etc/passwd",
            "a/../b",
            ".git/config",
            "a\\b",
            "C:x",
            "",
        ] {
            assert!(!safe_path(bad), "{bad}");
        }
        assert!(safe_path("references/a.md"));
    }
}

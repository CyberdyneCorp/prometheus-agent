//! Skill bundles (spec: skills, Skill bundles).
//!
//! A bundle is `<profile>/skill-bundles/<slug>.toml` naming several skills
//! and an optional instruction; `/<slug>` loads every listed skill that
//! resolves into one user message (never the system prompt). A missing
//! member is skipped and named in the message, never fatal. A file that
//! does not parse is reported, not loaded.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::digest;
use crate::lockfile::io;
use crate::session::SkillsSession;
use crate::slash::{self, Message};

/// The bundle directory below a profile.
pub const DIR: &str = "skill-bundles";
/// Most skills one bundle may list.
pub const MAX_SKILLS: usize = 32;

/// `<profile>/skill-bundles`.
#[must_use]
pub fn dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join(DIR)
}

/// One listed skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Member {
    /// Just a name.
    Name(String),
    /// A name with the hub identifier that installs it (M2 hub).
    Sourced {
        /// The skill name.
        name: String,
        /// Its hub identifier.
        source: Option<String>,
    },
}

impl Member {
    /// The skill name.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) | Self::Sourced { name, .. } => name,
        }
    }

    /// The hub identifier, if any.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::Sourced { source, .. } => source.as_deref(),
        }
    }
}

/// The file format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFile {
    /// Display name (default: the file stem).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// One line for listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The skills, in load order.
    pub skills: Vec<Member>,
    /// Added to the message after the skills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

/// A loaded bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// Its command (`/<slug>`).
    pub slug: String,
    /// Its name.
    pub name: String,
    /// The file contents.
    pub file: BundleFile,
    /// The file.
    pub path: PathBuf,
    /// Digest of the file bytes (reload reports changes with it).
    pub digest: String,
}

/// Every bundle of a profile.
#[derive(Debug, Clone, Default)]
pub struct Bundles {
    /// By slug.
    pub bundles: BTreeMap<String, Bundle>,
    /// Files that did not load: path and why.
    pub problems: Vec<(PathBuf, String)>,
}

impl Bundles {
    /// The bundle with command `slug`.
    #[must_use]
    pub fn get(&self, slug: &str) -> Option<&Bundle> {
        self.bundles.get(slug)
    }
}

/// Parses a bundle file named `stem`.
///
/// # Errors
///
/// Why the file is not a bundle.
pub fn parse(stem: &str, text: &str) -> Result<Bundle, String> {
    let file: BundleFile = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    if file.skills.is_empty() {
        return Err("`skills` must list at least one skill".to_owned());
    }
    if file.skills.len() > MAX_SKILLS {
        return Err(format!(
            "`skills` lists {} skills; at most {MAX_SKILLS} are allowed",
            file.skills.len()
        ));
    }
    if let Some(blank) = file.skills.iter().find(|m| m.name().trim().is_empty()) {
        return Err(format!("`skills` has an empty name ({blank:?})"));
    }
    let name = file.name.clone().unwrap_or_else(|| stem.to_owned());
    Ok(Bundle {
        slug: slash::slug(&name),
        name,
        file,
        path: PathBuf::new(),
        digest: digest::tree([(String::new(), text.as_bytes())]),
    })
}

/// Loads `<profile>/skill-bundles/*.toml`; a slug claimed twice keeps the
/// first file (by name) and reports the other.
#[must_use]
pub fn load(profile_dir: &Path) -> Bundles {
    let mut found = Bundles::default();
    let Ok(entries) = fs::read_dir(dir(profile_dir)) else {
        return found;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parsed = fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse(&stem, &text));
        match parsed {
            Ok(mut bundle) if !found.bundles.contains_key(&bundle.slug) => {
                bundle.path.clone_from(&path);
                found.bundles.insert(bundle.slug.clone(), bundle);
            }
            Ok(bundle) => found.problems.push((
                path,
                format!("slug '{}' is already taken by another bundle", bundle.slug),
            )),
            Err(why) => {
                tracing::warn!(path = %path.display(), reason = %why, "skill bundle not loaded");
                found.problems.push((path, why));
            }
        }
    }
    found
}

fn not_found(slug: &str) -> Error {
    Error::new(
        &codes::SKILL_BUNDLE_NOT_FOUND,
        format!("no skill bundle '{slug}'"),
    )
    .with_hint("prometheus skills bundles list")
}

/// The bundle `slug` of the profile.
///
/// # Errors
///
/// `skill.bundle_not_found`.
pub fn find(profile_dir: &Path, slug: &str) -> Result<Bundle, Error> {
    load(profile_dir)
        .bundles
        .remove(slug)
        .ok_or_else(|| not_found(slug))
}

/// Writes bundle `slug` (refused when it exists unless `force`).
///
/// # Errors
///
/// `skill.bundle_invalid` (bad slug, no skills, too many, exists),
/// `skill.io`.
pub fn create(
    profile_dir: &Path,
    slug: &str,
    file: &BundleFile,
    force: bool,
) -> Result<PathBuf, Error> {
    let invalid = |why: String| Error::new(&codes::SKILL_BUNDLE_INVALID, why);
    if slash::slug(slug) != slug || slug.is_empty() || slug.contains(['/', '\\', '.']) {
        return Err(invalid(format!(
            "'{slug}' is not a bundle slug (lower-case letters, digits and '-')"
        )));
    }
    let text = toml::to_string(file).map_err(|e| invalid(e.to_string()))?;
    parse(slug, &text).map_err(invalid)?;
    let dir = dir(profile_dir);
    let path = dir.join(format!("{slug}.toml"));
    if path.exists() && !force {
        return Err(invalid(format!(
            "bundle '{slug}' already exists at {}; pass --force to replace it",
            path.display()
        )));
    }
    private_fs::create_dir(&dir).map_err(|e| io(&dir, &e))?;
    private_fs::write_atomic(&path, text.as_bytes()).map_err(|e| io(&path, &e))?;
    Ok(path)
}

/// Removes bundle `slug`.
///
/// # Errors
///
/// `skill.bundle_not_found`, `skill.io`.
pub fn delete(profile_dir: &Path, slug: &str) -> Result<PathBuf, Error> {
    let bundle = find(profile_dir, slug)?;
    fs::remove_file(&bundle.path).map_err(|e| io(&bundle.path, &e))?;
    Ok(bundle.path)
}

/// The user message of `/<slug> args`: every listed skill that resolves,
/// the bundle instruction and the user's text. The stacking cap does not
/// apply; members that do not resolve are named as skipped.
pub async fn invoke(session: &SkillsSession, bundle: &Bundle, args: &str) -> String {
    let (catalog, config) = session.catalog();
    let mut message = Message {
        bundle_instruction: bundle.file.instruction.clone(),
        instruction: args.to_owned(),
        ..Message::default()
    };
    for member in &bundle.file.skills {
        match slash::resolve(session, &catalog, &config, member.name()) {
            Ok(skill) => message
                .blocks
                .push(slash::block(session, &skill, &config).await),
            Err(_) => message.skipped.push(member.name().to_owned()),
        }
    }
    message.render()
}

/// One line per bundle and per problem file (`/bundles`, `skills bundles
/// list`).
#[must_use]
pub fn listing(bundles: &Bundles) -> String {
    let mut text = String::new();
    for bundle in bundles.bundles.values() {
        let names: Vec<&str> = bundle.file.skills.iter().map(Member::name).collect();
        let _ = write!(text, "/{}  {}", bundle.slug, names.join(", "));
        if let Some(description) = &bundle.file.description {
            let _ = write!(text, "  ({description})");
        }
        text.push('\n');
    }
    for (path, why) in &bundles.problems {
        let _ = writeln!(text, "{}: {why}", path.display());
    }
    if text.is_empty() {
        text.push_str("No skill bundles.\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_defaults_to_the_stem_and_slugifies() {
        let bundle = parse("Backend_Dev", "skills = [\"a\"]\n").unwrap();
        assert_eq!(bundle.slug, "backend-dev");
        let named = parse(
            "x",
            "name = \"Ops Day\"\nskills = [\"a\", { name = \"b\", source = \"official/x/b\" }]\n",
        )
        .unwrap();
        assert_eq!(named.slug, "ops-day");
        assert_eq!(named.file.skills[1].source(), Some("official/x/b"));
    }

    #[test]
    fn empty_oversized_and_unknown_fields_are_refused() {
        assert!(parse("x", "skills = []\n").is_err());
        let many: Vec<String> = (0..33).map(|i| format!("\"s{i}\"")).collect();
        assert!(parse("x", &format!("skills = [{}]\n", many.join(","))).is_err());
        assert!(parse("x", "skills = [\"a\"]\ncolour = 1\n").is_err());
    }
}

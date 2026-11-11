//! The signed release notes of `release-manifest.json`, read from
//! `CHANGELOG.md` (spec: distribution-and-update, Update dry-run and
//! changelog preview): `## vX.Y.Z` headings (newest first), each with
//! `### Breaking`, `### Features`, `### Fixes` and `### Security`
//! subsections of `- ` bullets. `## Unreleased` belongs to a canary.

use pmt_update::ReleaseVersion;
use pmt_update::manifest::ReleaseNotes;

/// How many releases of history a manifest carries.
const KEEP: usize = 30;

/// The notes of `tag` and the releases before it, newest first.
pub(super) fn parse(changelog: &str, tag: ReleaseVersion) -> Result<Vec<ReleaseNotes>, String> {
    let mut all: Vec<ReleaseNotes> = Vec::new();
    let mut section: Option<String> = None;
    for line in changelog.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            let name = heading.split_whitespace().next().unwrap_or_default();
            let version = if name.eq_ignore_ascii_case("unreleased") {
                Some(tag).filter(|t| t.channel() == pmt_update::Channel::Canary)
            } else {
                Some(ReleaseVersion::parse(name).map_err(|e| format!("CHANGELOG.md: {e}"))?)
            };
            match version.filter(|v| *v <= tag) {
                Some(version) => all.push(empty(version)),
                None => all.push(empty(skipped())),
            }
            section = None;
        } else if let Some(name) = line.strip_prefix("### ") {
            section = Some(name.trim().to_ascii_lowercase());
        } else if let (Some(text), Some(notes), Some(section)) =
            (line.strip_prefix("- "), all.last_mut(), section.as_deref())
        {
            let list = match section {
                "breaking" => &mut notes.breaking,
                "features" => &mut notes.features,
                "fixes" => &mut notes.fixes,
                "security" => &mut notes.security,
                other => return Err(format!("CHANGELOG.md: unknown section '### {other}'")),
            };
            list.push(text.trim().to_owned());
        }
    }
    let skip = skipped();
    all.retain(|n| n.version != skip);
    all.truncate(KEEP);
    Ok(all)
}

fn empty(version: ReleaseVersion) -> ReleaseNotes {
    ReleaseNotes {
        version,
        breaking: Vec::new(),
        features: Vec::new(),
        fixes: Vec::new(),
        security: Vec::new(),
    }
}

/// Marks a heading whose entries are not part of this release (a newer
/// release, or `Unreleased` of a stable tag).
fn skipped() -> ReleaseVersion {
    ReleaseVersion::parse("v0.0.0").expect("valid version")
}

#[cfg(test)]
mod tests {
    use pmt_update::ReleaseVersion;

    use super::parse;

    const LOG: &str = "# Changelog\n\n## Unreleased\n### Features\n- next\n\n\
        ## v2.0.0 (2026-11-01)\n### Breaking\n- config v1 to v2\n### Fixes\n- a fix\n\n\
        ## v1.4.1\n### Security\n- patched\n";

    #[test]
    fn a_stable_tag_takes_its_release_and_older_ones() {
        let notes = parse(LOG, ReleaseVersion::parse("v2.0.0").unwrap()).unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].breaking, ["config v1 to v2"]);
        assert_eq!(notes[1].security, ["patched"]);
        let older = parse(LOG, ReleaseVersion::parse("v1.4.1").unwrap()).unwrap();
        assert_eq!(older.len(), 1);
    }

    #[test]
    fn a_canary_owns_the_unreleased_entries() {
        let tag = ReleaseVersion::parse("v2.1.0-canary.20261105000000").unwrap();
        let notes = parse(LOG, tag).unwrap();
        assert_eq!(notes[0].version, tag);
        assert_eq!(notes[0].features, ["next"]);
    }
}

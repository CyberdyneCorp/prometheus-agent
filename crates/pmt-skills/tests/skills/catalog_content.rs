//! The skills shipped in the binary (spec: skills, Bundled and official
//! skill catalog content): every one passes the strict authoring lint, the
//! required skills are present, and Hermes names resolve.

use std::collections::BTreeSet;

use pmt_skills::bundled::Bundled;
use pmt_skills::layout::SKILL_FILE;
use pmt_skills::lint::{self, LintContext};
use pmt_skills::{eject, official};

use crate::support::Profile;

fn names(tree: &Bundled) -> BTreeSet<String> {
    official::skills(tree)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn every_embedded_skill_passes_the_strict_lint() {
    let bundled = Bundled::builtin();
    let catalog = Bundled::official();
    let known: BTreeSet<String> = names(&bundled).union(&names(&catalog)).cloned().collect();
    let resolves = |name: &str| known.contains(name);
    let mut failures = Vec::new();
    for tree in [&bundled, &catalog] {
        for (name, prefix) in official::skills(tree) {
            let text =
                String::from_utf8_lossy(tree.read(&prefix, SKILL_FILE).unwrap()).into_owned();
            let ctx = LintContext {
                dir_name: &name,
                description_chars: 60,
                resolves: &resolves,
            };
            let found = lint::lint(&text, &ctx);
            if !found.is_empty() {
                failures.push(format!("{prefix}: {found:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_required_skills_are_embedded() {
    let bundled = names(&Bundled::builtin());
    for name in [
        "apple-notes",
        "imessage",
        "claude-code",
        "prometheus-agent",
        "p5js",
        "sdlc-review",
        "himalaya",
        "youtube-content",
        "obsidian",
        "pdf",
        "arxiv",
        "xurl",
        "github",
        "test-driven-development",
        "blocked-page-recovery",
    ] {
        assert!(bundled.contains(name), "bundled {name}");
    }
    assert_eq!(bundled.len(), 44, "{bundled:?}");
    let catalog = official::skills(&Bundled::official());
    let paths: BTreeSet<String> = catalog.iter().map(|(_, p)| p.clone()).collect();
    for path in [
        "mlops/axolotl",
        "software-development/prometheus-skill-authoring",
        "devops/prometheus-s6-container-supervision",
        "yuanbao/yuanbao",
        "security/1password",
    ] {
        assert!(paths.contains(path), "official {path}");
    }
    assert_eq!(catalog.len(), 149);
    // Official skills are never active until installed.
    assert!(!bundled.contains("axolotl"));
}

#[test]
fn hermes_name_alias_resolves() {
    let profile = Profile::new("");
    let catalog = eject::profile_catalog(&profile.scope, None, None, Bundled::builtin());
    assert_eq!(
        catalog.lookup("hermes-agent").unwrap().name,
        "prometheus-agent"
    );
    assert_eq!(
        pmt_skills::catalog::hermes_alias("hermes-agent-skill-authoring").as_deref(),
        Some("prometheus-skill-authoring")
    );
    assert!(catalog.lookup("hermes-nonexistent").is_err());
}

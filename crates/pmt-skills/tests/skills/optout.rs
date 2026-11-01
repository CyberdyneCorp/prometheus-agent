//! Bundled skill opt-out and official catalog repair (spec: skills,
//! Bundled skill opt-out and official catalog repair).

use std::sync::Arc;

use pmt_skills::bundled::Bundled;
use pmt_skills::lockfile::{Contents, LockFile};
use pmt_skills::official::{self, Action, Options};
use pmt_skills::roots::Tier;
use pmt_skills::{eject, optout};

use crate::support::{Profile, Session, skill_md, write};

fn tree(skills: &[(&str, &str)]) -> Arc<Bundled> {
    Arc::new(Bundled::from_files(skills.iter().map(|(dir, name)| {
        (
            format!("{dir}/SKILL.md"),
            skill_md(name, &format!("The {name} skill."), ""),
        )
    })))
}

const V1: &[(&str, &str)] = &[
    ("autonomous-ai-agents/prometheus-agent", "prometheus-agent"),
    ("devops/sdlc-review", "sdlc-review"),
];
const V2: &[(&str, &str)] = &[
    ("autonomous-ai-agents/prometheus-agent", "prometheus-agent"),
    ("devops/sdlc-review", "sdlc-review"),
    ("research/arxiv", "arxiv"),
];

/// The bundled skills `skills_list` shows and the frozen index text.
async fn bundled_names(profile: &Profile, bundled: Arc<Bundled>) -> (Vec<String>, String) {
    let mut setup = profile.setup(profile.session());
    setup.bundled = bundled;
    let session = Session::start(profile, setup).await;
    let listed = session.json("skills_list", serde_json::json!({})).await;
    let mut names: Vec<String> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["source"] == "bundled")
        .map(|s| s["name"].as_str().unwrap().to_owned())
        .collect();
    names.sort();
    (names, session.skills.index_text().unwrap_or_default())
}

#[tokio::test]
async fn blank_slate_profile_stays_blank_across_upgrades() {
    let blank = Profile::new("[skills]\ninclude_bundled = false\n");
    assert_eq!(
        bundled_names(&blank, tree(V1)).await.0,
        ["prometheus-agent"]
    );
    // The upgrade ships a new bundled skill.
    let (names, index) = bundled_names(&blank, tree(V2)).await;
    assert_eq!(names, ["prometheus-agent"]);
    assert!(
        index.contains("prometheus-agent") && !index.contains("arxiv"),
        "{index}"
    );
    let regular = blank.sibling("work", "");
    assert_eq!(
        bundled_names(&regular, tree(V2)).await.0,
        ["arxiv", "prometheus-agent", "sdlc-review"]
    );
}

#[test]
fn opt_out_remove_keeps_edited_copies() {
    let profile = Profile::new("");
    let bundled = tree(V2);
    let home = Some(profile.path("user-home"));
    let catalog =
        |p: &Profile| eject::profile_catalog(&p.scope, None, home.as_deref(), Arc::clone(&bundled));
    eject::eject(profile.scope.dir(), &catalog(&profile), "sdlc-review").unwrap();
    let edited = eject::eject(profile.scope.dir(), &catalog(&profile), "arxiv").unwrap();
    write(
        &edited.path,
        "SKILL.md",
        &skill_md("arxiv", "My arxiv.", ""),
    );
    write(
        &profile.skills(),
        "mine/SKILL.md",
        &skill_md("mine", "Mine.", ""),
    );

    let modified = optout::modified(profile.scope.dir(), &catalog(&profile));
    let names: Vec<&str> = modified.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["arxiv"]);

    let removed = optout::remove_identical(profile.scope.dir(), &catalog(&profile)).unwrap();
    assert_eq!(removed, ["sdlc-review"]);
    assert!(edited.path.join("SKILL.md").is_file(), "edited copy kept");
    assert!(
        profile.skills().join("mine/SKILL.md").is_file(),
        "user skill kept"
    );
    let Contents::Entries(entries) = LockFile::in_dir(&profile.skills()).read() else {
        panic!("lockfile unreadable");
    };
    assert!(entries.iter().all(|e| e.name != "sdlc-review"));

    optout::set_included(profile.scope.profile(), false).unwrap();
    let config = profile
        .scope
        .profile()
        .config_file()
        .raw()
        .unwrap()
        .unwrap();
    assert!(config.contains("include_bundled = false"), "{config}");
    optout::set_included(profile.scope.profile(), true).unwrap();
    let config = profile
        .scope
        .profile()
        .config_file()
        .raw()
        .unwrap()
        .unwrap();
    assert!(config.contains("include_bundled = true"), "{config}");
}

fn official_tree() -> Bundled {
    Bundled::from_files([
        (
            "mlops/axolotl/SKILL.md",
            skill_md("axolotl", "Fine-tune with axolotl.", ""),
        ),
        (
            "mlops/axolotl/references/config.md",
            "lr: 1e-4\n".to_owned(),
        ),
        (
            "research/arxiv-search/SKILL.md",
            skill_md("arxiv-search", "Search arXiv.", ""),
        ),
    ])
}

fn lock_source(profile: &Profile, name: &str) -> Option<String> {
    match LockFile::in_dir(&profile.skills()).read() {
        Contents::Entries(entries) => entries
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.source),
        Contents::Unreadable(_) => None,
    }
}

#[test]
fn repair_official_records_and_restores() {
    let profile = Profile::new("");
    let official = official_tree();
    let home = Some(profile.path("user-home"));
    let catalog = || {
        eject::profile_catalog(
            &profile.scope,
            None,
            home.as_deref(),
            Arc::new(Bundled::default()),
        )
    };
    // An install whose lockfile entry was lost.
    write(
        &profile.skills(),
        "mlops/axolotl/SKILL.md",
        &skill_md("axolotl", "Fine-tune with axolotl.", ""),
    );
    write(
        &profile.skills(),
        "mlops/axolotl/references/config.md",
        "lr: 1e-4\n",
    );
    let all = |restore| Options {
        target: "all",
        restore,
    };

    let repaired =
        official::repair(profile.scope.dir(), &catalog(), &official, all(false)).unwrap();
    assert_eq!(repaired.len(), 1, "only installed official skills");
    assert_eq!(repaired[0].action, Action::Recorded);
    assert_eq!(
        lock_source(&profile, "axolotl").as_deref(),
        Some("official/mlops/axolotl")
    );
    let skill = catalog().find("axolotl").cloned().unwrap();
    assert!(skill.provenance.is_hub() && skill.tier == Tier::Profile);

    write(
        &profile.skills(),
        "mlops/axolotl/references/config.md",
        "lr: 9\n",
    );
    let modified =
        official::repair(profile.scope.dir(), &catalog(), &official, all(false)).unwrap();
    assert_eq!(modified[0].action, Action::Modified);
    let restored = official::repair(profile.scope.dir(), &catalog(), &official, all(true)).unwrap();
    assert_eq!(restored[0].action, Action::Restored);
    let backup = restored[0].backup.clone().unwrap();
    assert!(backup.starts_with(profile.skills().join(".backups")));
    assert_eq!(
        std::fs::read_to_string(backup.join("references/config.md")).unwrap(),
        "lr: 9\n"
    );
    assert_eq!(
        std::fs::read_to_string(profile.skills().join("mlops/axolotl/references/config.md"))
            .unwrap(),
        "lr: 1e-4\n"
    );

    let one = |restore| Options {
        target: "arxiv-search",
        restore,
    };
    let missing = official::repair(profile.scope.dir(), &catalog(), &official, one(false)).unwrap();
    assert_eq!(missing[0].action, Action::Missing);
    let installed =
        official::repair(profile.scope.dir(), &catalog(), &official, one(true)).unwrap();
    assert_eq!(installed[0].action, Action::Restored);
    assert!(
        profile
            .skills()
            .join("research/arxiv-search/SKILL.md")
            .is_file()
    );
    let err = official::repair(
        profile.scope.dir(),
        &catalog(),
        &official,
        Options {
            target: "nope",
            restore: false,
        },
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 66);
}

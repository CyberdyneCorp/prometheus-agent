//! 17.1: SKILL.md format against agentskills.io fixtures, layout and
//! exclusions, search roots and precedence, bundled skills and eject.

use std::sync::Arc;

use pmt_skills::bundled::Bundled;
use pmt_skills::eject;
use pmt_skills::lockfile::{Contents, LockFile};
use pmt_skills::roots::Tier;
use serde_json::json;

use crate::support::{Profile, Session, add_skill, copy_fixtures, skill_md, write};

#[tokio::test]
async fn agentskills_fixtures_load_with_optional_fields_passed_through() {
    let profile = Profile::new("");
    copy_fixtures(&profile.skills());
    let session = Session::open(&profile).await;
    let listed = session.json("skills_list", json!({})).await;
    let names: Vec<&str> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["pdf-processing", "foo", "axolotl", "brand-guidelines"]
    );
    assert_eq!(listed["count"], 4);
    let (catalog, _) = session.skills.catalog();
    let pdf = catalog.find("pdf-processing").unwrap();
    assert_eq!(pdf.category, "document-processing");
    assert_eq!(pdf.front["license"], "Apache-2.0");
    assert_eq!(pdf.front["allowed-tools"], "Bash(python3:*) Read");
    assert_eq!(
        pdf.front["compatibility"],
        "Requires python3 with pdfplumber installed."
    );
    assert_eq!(
        pdf.front["metadata"],
        json!({"author": "example-org", "version": "1.0"})
    );
    let view = session
        .json("skill_view", json!({"name": "pdf-processing"}))
        .await;
    assert_eq!(
        view["linked_files"],
        json!({"references": ["references/REFERENCE.md"], "scripts": ["scripts/extract.py"]})
    );
    let reference = session
        .json("skill_view", json!({"name": "document-processing/pdf-processing", "file_path": "references/REFERENCE.md"}))
        .await;
    assert!(
        reference["content"]
            .as_str()
            .unwrap()
            .starts_with("# Form filling")
    );
}

#[tokio::test]
async fn hermes_skill_loads_unchanged() {
    let profile = Profile::new("");
    copy_fixtures(&profile.skills());
    let path = profile.skills().join("mlops/axolotl/SKILL.md");
    let before = std::fs::read(&path).unwrap();
    let session = Session::open(&profile).await;
    let (catalog, _) = session.skills.catalog();
    let axolotl = catalog.find("axolotl").unwrap();
    assert_eq!(axolotl.meta.tags, ["mlops"]);
    assert_eq!(axolotl.meta.related_skills, ["pdf-processing"]);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the file was modified"
    );
}

#[tokio::test]
async fn malformed_frontmatter_is_reported_not_guessed() {
    let profile = Profile::new("");
    copy_fixtures(&profile.skills());
    let session = Session::open(&profile).await;
    let (catalog, _) = session.skills.catalog();
    assert!(catalog.find("broken").is_none());
    let problem = catalog
        .problems
        .iter()
        .find(|p| p.path.ends_with("broken/SKILL.md"))
        .expect("reported");
    assert!(
        problem.reason.starts_with("invalid frontmatter"),
        "{problem:?}"
    );
    assert!(problem.line >= 4, "{problem:?}");
    let listed = session.text("skills_list", json!({})).await;
    assert!(!listed.contains("broken"));
    assert!(
        session
            .text("skill_view", json!({"name": "broken"}))
            .await
            .contains("not found")
    );
}

#[tokio::test]
async fn nested_skill_md_in_references_is_ignored() {
    let profile = Profile::new("");
    copy_fixtures(&profile.skills());
    let session = Session::open(&profile).await;
    let (catalog, _) = session.skills.catalog();
    assert!(catalog.find("foo").is_some());
    assert!(catalog.find("example").is_none());
}

#[tokio::test]
async fn excluded_directories_are_not_scanned() {
    let profile = Profile::new("");
    let skills = profile.skills();
    for dir in [
        ".git",
        "node_modules/pkg",
        ".archive/old",
        "venv/lib/site-packages/x",
        "dev/.backups",
    ] {
        add_skill(&skills, &format!("{dir}/hidden"), "hidden", "Never found.");
    }
    add_skill(&skills, "dev/shown", "shown", "Found.");
    let session = Session::open(&profile).await;
    let (catalog, _) = session.skills.catalog();
    let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["shown"]);
}

#[tokio::test]
async fn profile_skill_shadows_bundled_skill() {
    let profile = Profile::new("");
    add_skill(
        &profile.skills(),
        "dev/git-workflow",
        "git-workflow",
        "Profile copy.",
    );
    let mut setup = profile.setup(profile.session());
    setup.bundled = Arc::new(Bundled::from_files([(
        "dev/git-workflow/SKILL.md",
        skill_md("git-workflow", "Bundled copy.", "").into_bytes(),
    )]));
    let session = Session::start(&profile, setup).await;
    let view = session
        .json("skill_view", json!({"name": "git-workflow"}))
        .await;
    assert_eq!(view["description"], "Profile copy.");
    assert_eq!(view["source"], "profile");
    let (catalog, _) = session.skills.catalog();
    assert_eq!(catalog.shadowed.len(), 1);
    assert_eq!(catalog.shadowed[0].tier, Tier::Bundled);
}

#[tokio::test]
async fn same_tier_collision_is_refused() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "a/deploy", "deploy", "First.");
    add_skill(&profile.skills(), "b/deploy", "deploy", "Second.");
    let session = Session::open(&profile).await;
    let result = session.call("skill_view", json!({"name": "deploy"})).await;
    let text = result.text_content();
    assert!(!result.status.succeeded());
    assert!(text.starts_with("Ambiguous skill name 'deploy'"), "{text}");
    assert!(
        text.contains("a/deploy") && text.contains("b/deploy"),
        "{text}"
    );
    assert!(!text.contains("First.") && !text.contains("Second."));
}

#[tokio::test]
async fn tiers_follow_one_precedence_order() {
    let profile = Profile::new("");
    let external = profile.path("external");
    add_skill(&external, "shared", "shared", "External copy.");
    add_skill(&external, "ext-only", "ext-only", "Only external.");
    add_skill(&profile.skills(), "shared", "shared", "Profile copy.");
    let missing = profile.root.path().join("nope");
    let mut profile = profile;
    profile.reconfigure(&format!(
        "[skills]\nexternal_dirs = [{:?}, {:?}]\n",
        missing.display().to_string(),
        external.display().to_string()
    ));
    let session = Session::open(&profile).await;
    let (catalog, _) = session.skills.catalog();
    assert_eq!(catalog.find("shared").unwrap().description, "Profile copy.");
    assert_eq!(catalog.find("ext-only").unwrap().tier, Tier::External);
    assert_eq!(catalog.shadowed[0].description, "External copy.");
}

fn bundled_git(version: &str) -> Bundled {
    Bundled::from_files([
        (
            "dev/git-workflow/SKILL.md".to_owned(),
            skill_md("git-workflow", version, "").into_bytes(),
        ),
        (
            "dev/git-workflow/references/tips.md".to_owned(),
            b"tips".to_vec(),
        ),
    ])
}

#[tokio::test]
async fn bundled_skills_are_served_read_only_without_copying() {
    let profile = Profile::new("");
    let mut setup = profile.setup(profile.session());
    setup.bundled = Arc::new(bundled_git("Bundled v1."));
    let session = Session::start(&profile, setup).await;
    let view = session
        .json("skill_view", json!({"name": "git-workflow"}))
        .await;
    assert_eq!(view["source"], "bundled");
    let tips = session
        .json(
            "skill_view",
            json!({"name": "git-workflow", "file_path": "references/tips.md"}),
        )
        .await;
    assert_eq!(tips["content"], "tips");
    assert!(
        !profile.skills().join("dev").exists(),
        "nothing copied into the profile"
    );
    let edit = session
        .text("skill_manage", json!({"action": "patch", "name": "git-workflow", "old_string": "Steps.", "new_string": "x"}))
        .await;
    assert!(edit.contains("bundled and read-only"), "{edit}");
}

#[tokio::test]
async fn upgrade_never_overwrites_a_user_edit() {
    let profile = Profile::new("");
    let mut setup = profile.setup(profile.session());
    setup.bundled = Arc::new(bundled_git("Bundled v1."));
    let v1 = Session::start(&profile, setup.clone()).await;
    let (catalog, _) = v1.skills.catalog();
    let ejected = eject::eject(profile.scope.dir(), &catalog, "git-workflow").unwrap();
    assert_eq!(ejected.path, profile.skills().join("dev/git-workflow"));
    let Contents::Entries(entries) = LockFile::in_dir(&profile.skills()).read() else {
        panic!("lockfile unreadable");
    };
    assert_eq!(entries[0].source, "bundled:dev/git-workflow");
    assert_eq!(
        entries[0].content_digest,
        bundled_git("Bundled v1.").digest("dev/git-workflow")
    );
    let skill_file = ejected.path.join("SKILL.md");
    let edited = skill_md("git-workflow", "My own version.", "");
    write(&ejected.path, "SKILL.md", &edited);
    // A new release ships a changed bundled skill.
    let v2_tree = bundled_git("Bundled v2.");
    setup.bundled = Arc::new(v2_tree.clone());
    let v2 = Session::start(&profile, setup).await;
    let view = v2.json("skill_view", json!({"name": "git-workflow"})).await;
    assert_eq!(view["description"], "My own version.");
    assert_eq!(std::fs::read_to_string(&skill_file).unwrap(), edited);
    let outdated = eject::outdated(profile.scope.dir(), &v2_tree);
    assert_eq!(outdated.len(), 1);
    assert_eq!(outdated[0].name, "git-workflow");
    let (catalog, _) = v2.skills.catalog();
    let diff = eject::diff(profile.scope.dir(), &catalog, &v2_tree, "git-workflow").unwrap();
    assert!(
        diff.contains("-description: Bundled v2.")
            && diff.contains("+description: My own version."),
        "{diff}"
    );
    assert!(eject::outdated(profile.scope.dir(), &bundled_git("Bundled v1.")).is_empty());
}

#[tokio::test]
async fn deleted_ejected_copy_falls_back_to_bundled() {
    let profile = Profile::new("");
    let mut setup = profile.setup(profile.session());
    setup.bundled = Arc::new(bundled_git("Bundled v1."));
    let first = Session::start(&profile, setup.clone()).await;
    let (catalog, _) = first.skills.catalog();
    let ejected = eject::eject(profile.scope.dir(), &catalog, "git-workflow").unwrap();
    std::fs::remove_dir_all(&ejected.path).unwrap();
    setup.session_id = profile.session().to_string();
    let next = Session::start(&profile, setup).await;
    let view = next
        .json("skill_view", json!({"name": "git-workflow"}))
        .await;
    assert_eq!(view["source"], "bundled");
}

#[tokio::test]
async fn reset_drops_the_ejected_copy() {
    let profile = Profile::new("");
    let mut setup = profile.setup(profile.session());
    setup.bundled = Arc::new(bundled_git("Bundled v1."));
    let session = Session::start(&profile, setup).await;
    let (catalog, _) = session.skills.catalog();
    eject::eject(profile.scope.dir(), &catalog, "git-workflow").unwrap();
    let (catalog, _) = session.skills.catalog();
    assert!(eject::eject(profile.scope.dir(), &catalog, "git-workflow").is_err());
    eject::reset(profile.scope.dir(), &catalog, "git-workflow").unwrap();
    assert!(!profile.skills().join("dev/git-workflow").exists());
    let view = session
        .json("skill_view", json!({"name": "git-workflow"}))
        .await;
    assert_eq!(view["source"], "bundled");
}

//! The skills CLI through the binary (spec: skills, Skill authoring lint;
//! Disabled skills; Skill bundles; /reload-skills rescans roots; Skill
//! write-approval review surface; Bundled skill opt-out and official
//! catalog repair; spec: cli). Listings are compared with snapshots in
//! `tests/snapshots/`.

mod common;

use std::path::Path;
use std::process::{Output, Stdio};

use common::{command, ok, stderr, stdout, temp_home};
use serde_json::Value;

fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name}");
}

/// Runs `args` from inside the home (no project skills of the checkout).
fn run(home: &Path, args: &[&str]) -> Output {
    command(home)
        .args(args)
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill(home: &Path, rel: &str, name: &str, description: &str) {
    write(
        &home
            .join("profiles/default/skills")
            .join(rel)
            .join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n\nSteps.\n"),
    );
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

#[test]
fn list_shows_source_status_and_shadowed_copies() {
    let home = temp_home();
    let h = home.path();
    skill(h, "devops/deploy", "deploy", "Deploy the service.");
    skill(h, "notes/journal", "journal", "Write a journal entry.");
    skill(h, "notes/old-notes", "old-notes", "Old notes.");
    skill(
        h,
        "autonomous-ai-agents/prometheus-agent",
        "prometheus-agent",
        "My copy.",
    );
    assert!(run(h, &["skills", "disable", "old-notes"]).status.success());
    let out = run(
        h,
        &["skills", "disable", "journal", "--platform", "telegram"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("Disabled skill 'journal' on telegram"));
    snapshot("skills-list.txt", &ok(&run(h, &["skills", "list"])));
    let doc = json(h, &["skills", "list"]);
    assert_eq!(doc["schema"], "prometheus.skills.v1");
    let rows = doc["skills"].as_array().unwrap();
    let bundled = rows
        .iter()
        .find(|r| r["name"] == "prometheus-agent" && r["source"] == "bundled")
        .unwrap();
    assert_eq!(bundled["status"], "shadowed");
    assert!(run(h, &["skills", "enable", "old-notes"]).status.success());
    let doc = json(h, &["skills", "list"]);
    let old = doc["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "old-notes")
        .unwrap()
        .clone();
    assert_eq!(old["status"], "enabled");
}

#[test]
fn show_prints_origin_status_and_content() {
    let home = temp_home();
    let text = ok(&run(home.path(), &["skills", "show", "prometheus-agent"]));
    let header: String = text.lines().take(7).collect::<Vec<_>>().join("\n") + "\n";
    snapshot("skills-show-essential.txt", &header);
    assert!(text.contains("## Procedure"));
    let doc = json(home.path(), &["skills", "show", "prometheus-agent"]);
    assert_eq!(doc["schema"], "prometheus.skill.v1");
    assert_eq!(doc["source"], "bundled");
    let missing = run(home.path(), &["skills", "show", "nope"]);
    assert_eq!(missing.status.code(), Some(66));
}

#[test]
fn marketing_word_fails_strict_lint() {
    let home = temp_home();
    let dir = home.path().join("work/logs");
    write(
        &dir.join("SKILL.md"),
        "---\nname: logs\ndescription: Advanced log analysis.\n---\n\nSteps.\n",
    );
    let strict = run(
        home.path(),
        &["skills", "lint", "--strict", dir.to_str().unwrap()],
    );
    assert_eq!(strict.status.code(), Some(1), "{}", stderr(&strict));
    let line = stdout(&strict);
    assert!(
        line.contains("banned_word: description uses 'advanced'"),
        "{line}"
    );
    let advisory = run(home.path(), &["skills", "lint", dir.to_str().unwrap()]);
    assert_eq!(advisory.status.code(), Some(0));
    let mixed = home.path().join("work");
    write(
        &mixed.join("tidy/SKILL.md"),
        "---\nname: tidy\ndescription: Keep a tidy tree.\n---\n\nSteps.\n",
    );
    write(
        &mixed.join("wrong/SKILL.md"),
        "---\nname: right\ndescription: No period\n---\n\nSee /home/alice/notes.\n",
    );
    let out = run(
        home.path(),
        &["skills", "lint", "--strict", mixed.to_str().unwrap()],
    );
    let text = stdout(&out).replace(&mixed.display().to_string(), "<dir>");
    snapshot("skills-lint.txt", &text);
}

#[test]
fn bundled_skills_pass_strict_lint() {
    let home = temp_home();
    let out = run(home.path(), &["skills", "lint", "--strict"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
}

#[test]
fn essential_skill_cannot_be_disabled_even_per_platform() {
    let home = temp_home();
    for args in [
        &["skills", "disable", "prometheus-agent"][..],
        &[
            "skills",
            "disable",
            "prometheus-agent",
            "--platform",
            "telegram",
        ][..],
    ] {
        let out = run(home.path(), args);
        assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    }
}

#[test]
fn bundles_are_created_listed_reloaded_and_deleted() {
    let home = temp_home();
    let h = home.path();
    let out = run(
        h,
        &[
            "skills",
            "bundles",
            "create",
            "backend-dev",
            "--skill",
            "prometheus-agent",
            "--skill",
            "skill-foo",
            "--instruction",
            "Run the tests.",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let again = run(
        h,
        &["skills", "bundles", "create", "backend-dev", "--skill", "x"],
    );
    assert_eq!(again.status.code(), Some(65), "exists without --force");
    assert_eq!(
        ok(&run(h, &["skills", "bundles", "list"])),
        "/backend-dev  prometheus-agent, skill-foo\n"
    );
    let show = ok(&run(h, &["skills", "bundles", "show", "backend-dev"]));
    assert!(
        show.contains("skill: skill-foo (missing)") && show.contains("instruction: Run the tests."),
        "{show}"
    );
    assert_eq!(
        ok(&run(h, &["skills", "bundles", "reload"])),
        "Skills reloaded.\nbundles added: backend-dev\n"
    );
    write(
        &h.join("profiles/default/skill-bundles/ops.toml"),
        "skills = [{ name = \"axolotl\", source = \"official/mlops/axolotl\" }]\n",
    );
    let install = run(h, &["skills", "bundles", "install", "ops"]);
    assert_eq!(install.status.code(), Some(69), "{}", stderr(&install));
    write(
        &h.join("profiles/default/skill-bundles/broken.toml"),
        "skills = []\n",
    );
    let listing = ok(&run(h, &["skills", "bundles", "list"]));
    assert!(
        listing.contains("broken.toml: `skills` must list at least one skill"),
        "{listing}"
    );
    assert!(
        run(h, &["skills", "bundles", "delete", "backend-dev"])
            .status
            .success()
    );
    assert_eq!(
        run(h, &["skills", "bundles", "show", "backend-dev"])
            .status
            .code(),
        Some(66)
    );
}

#[test]
fn reload_reports_changes_since_the_last_reload() {
    let home = temp_home();
    let h = home.path();
    ok(&run(h, &["skills", "reload"]));
    skill(h, "ops/new-one", "new-one", "A new skill.");
    assert_eq!(
        ok(&run(h, &["skills", "reload"])),
        "Skills reloaded.\nskills added: new-one\n"
    );
    assert_eq!(
        ok(&run(h, &["skills", "reload"])),
        "Skills reloaded: no changes.\n"
    );
}

#[test]
fn blank_slate_profile_lists_only_the_essential_bundled_skill() {
    let home = temp_home();
    let h = home.path();
    let out = run(h, &["profile", "create", "blank", "--no-skills"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let doc = json(h, &["-p", "blank", "skills", "list"]);
    let bundled: Vec<&str> = doc["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["source"] == "bundled")
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(bundled, ["prometheus-agent"]);
    assert!(
        run(h, &["-p", "blank", "skills", "opt-in"])
            .status
            .success()
    );
    assert_eq!(
        ok(&run(
            h,
            &["-p", "blank", "config", "get", "skills.include_bundled"]
        )),
        "true\n"
    );
    assert!(
        run(h, &["-p", "blank", "skills", "opt-out"])
            .status
            .success()
    );
    assert_eq!(
        ok(&run(
            h,
            &["-p", "blank", "config", "get", "skills.include_bundled"]
        )),
        "false\n"
    );
}

#[test]
fn maintenance_commands_on_an_empty_profile() {
    let home = temp_home();
    let h = home.path();
    assert_eq!(
        ok(&run(h, &["skills", "list-modified"])),
        "no modified ejected skills\n"
    );
    assert_eq!(
        ok(&run(h, &["skills", "repair-official", "all"])),
        "no official skills installed\n"
    );
    let restore = run(h, &["skills", "repair-official", "all", "--restore"]);
    assert_eq!(
        restore.status.code(),
        Some(4),
        "needs --yes without a terminal"
    );
    assert_eq!(
        ok(&run(h, &["skills", "pending"])),
        "No pending skill writes.\n"
    );
    assert_eq!(
        run(h, &["skills", "pending", "diff", "NOPE"]).status.code(),
        Some(66)
    );
    assert_eq!(
        ok(&run(h, &["skills", "list", "--outdated"])),
        "no outdated skills\n"
    );
}

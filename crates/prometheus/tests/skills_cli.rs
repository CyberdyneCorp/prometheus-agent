//! `prometheus skills trust|eject|doctor` (spec: skills, SKILL.md format:
//! malformed frontmatter is reported by `skills doctor`; Project-local
//! skills require explicit trust; Bundled skills are read-only and
//! ejectable).

mod common;

use std::path::Path;

use common::{run, temp_home};

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn doctor_reports_malformed_frontmatter_with_path_and_line() {
    let home = temp_home();
    assert!(run(home.path(), &["config", "path"]).status.success());
    let skills = home.path().join("profiles/default/skills");
    write(
        &skills.join("broken/SKILL.md"),
        "---\nname: broken\ndescription: Never closes.\ntags: [a, b\n---\nBody.\n",
    );
    write(
        &skills.join("fine/SKILL.md"),
        "---\nname: fine\ndescription: Loads.\n---\nBody.\n",
    );
    let out = run(home.path(), &["skills", "doctor"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    let line = text.lines().find(|l| l.contains("broken")).unwrap();
    let path = skills.join("broken/SKILL.md");
    assert!(line.starts_with(&format!("{}:", path.display())), "{line}");
    assert!(line.contains(": invalid frontmatter"), "{line}");
    let number: usize = line[path.display().to_string().len() + 1..]
        .split(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(number >= 4, "{line}");
    assert!(!text.contains("fine"));
    std::fs::remove_dir_all(skills.join("broken")).unwrap();
    let clean = run(home.path(), &["skills", "doctor"]);
    assert_eq!(clean.status.code(), Some(0));
    assert_eq!(stdout(&clean), "no skill problems\n");
}

#[test]
fn trust_records_the_canonical_root() {
    let home = temp_home();
    let repo = home.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let out = run(home.path(), &["skills", "trust", repo.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let canonical = repo.canonicalize().unwrap();
    assert!(stderr(&out).contains(&format!("Trusted {}", canonical.display())));
    let value = run(
        home.path(),
        &["config", "get", "skills.trusted_project_dirs"],
    );
    assert!(
        stdout(&value).contains(&canonical.display().to_string()),
        "{}",
        stdout(&value)
    );
    // Twice is the same as once.
    run(home.path(), &["skills", "trust", repo.to_str().unwrap()]);
    let again = stdout(&run(
        home.path(),
        &["config", "get", "skills.trusted_project_dirs"],
    ));
    assert_eq!(
        again.matches(&canonical.display().to_string()).count(),
        1,
        "{again}"
    );
}

#[test]
fn eject_of_an_unknown_bundled_skill_is_not_found() {
    let home = temp_home();
    let out = run(home.path(), &["skills", "eject", "no-such-skill"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    assert!(stderr(&out).contains("no bundled skill named 'no-such-skill'"));
}

#[test]
fn disabling_the_essential_skill_is_refused_by_config_set() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["config", "set", "skills.disabled", "[\"prometheus-agent\"]"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("skills.disabled"));
    assert!(stderr(&out).contains("prometheus-agent"));
}

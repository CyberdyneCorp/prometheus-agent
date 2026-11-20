//! Skill evals and eval-gated edits through the binary (spec: skills,
//! Skill evals; Eval-gated skill edits): every case runs in a temporary
//! home of its own, on the mock provider playing the case's script or on
//! the profile's model (`--provider live`).

mod common;

use std::path::{Path, PathBuf};

use common::{append_config, mock_home, run, stderr, stdout};
use serde_json::Value;

fn skill_dir(home: &Path, name: &str) -> PathBuf {
    home.join("profiles/default/skills").join(name)
}

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A mock script that loads `skill`, then answers `word` when the
/// skill's text holds it and `nothing` otherwise.
fn loads_then_says(skill: &str, word: &str) -> String {
    format!(
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"skill_view\"\narguments = {{ name = \"{skill}\" }}\n\n\
         [[rule]]\ntool_result = \"{word}\"\ntext = \"{word}\"\n\n[[rule]]\ntext = \"nothing\"\n"
    )
}

fn csv_tools(home: &Path) {
    let dir = skill_dir(home, "csv-tools");
    write(
        &dir,
        "SKILL.md",
        "---\nname: csv-tools\ndescription: Work with CSV files.\n---\nSum a column with the total 42.\n",
    );
    write(
        &dir,
        "evals/evals.toml",
        "[[case]]\nname = \"sums\"\nprompt = \"sum column B of data.csv\"\nsetup_files = [\"data.csv\"]\n\
         expect_skill_loaded = true\nexpect_output_contains = \"42\"\nmock_script = \"mock/sums.toml\"\n",
    );
    write(&dir, "evals/data.csv", "A,B\n1,40\n2,2\n");
    write(
        &dir,
        "evals/mock/sums.toml",
        &loads_then_says("csv-tools", "42"),
    );
}

fn reports(home: &Path) -> Vec<Value> {
    let dir = home.join("profiles/default/evals/skills");
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).unwrap())
        .collect()
}

#[test]
fn a_non_triggering_description_fails_with_skill_not_loaded_on_the_live_model() {
    // Scenario: Skill eval detects a non-triggering description.
    let home = mock_home();
    csv_tools(home.path());
    // The profile's model answers without ever loading the skill.
    let script = home.path().join("live.toml");
    std::fs::write(&script, "[[rule]]\nrepeat = true\ntext = \"I cannot.\"\n").unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            script.to_string_lossy()
        ),
    );
    let out = run(
        home.path(),
        &["skills", "test", "csv-tools", "--provider", "live"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("FAIL sums"), "{text}");
    assert!(text.contains("skill not loaded"), "{text}");
    let saved = reports(home.path());
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["provider"], "live");
    assert_eq!(saved[0]["skills"][0]["cases"][0]["passed"], false);
}

#[test]
fn the_mock_provider_runs_cases_offline_with_their_files_and_repeats() {
    let home = mock_home();
    csv_tools(home.path());
    let out = run(
        home.path(),
        &["--json", "skills", "test", "csv-tools", "--repeat", "2"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let report: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["passed"], true, "{report}");
    let case = &report["skills"][0]["cases"][0];
    assert_eq!(
        (case["runs"].as_u64(), case["passed_runs"].as_u64()),
        (Some(2), Some(2))
    );
    // Temporary homes are gone; only the reports stay.
    let work = home.path().join("profiles/default/evals/skills/.work");
    assert!(std::fs::read_dir(&work).map_or(true, |mut d| d.next().is_none()));
    // A skill without evals is a usage error; --all skips it.
    write(
        &skill_dir(home.path(), "plain"),
        "SKILL.md",
        "---\nname: plain\ndescription: Plain.\n---\nNothing.\n",
    );
    assert!(
        !run(home.path(), &["skills", "test", "plain"])
            .status
            .success()
    );
    assert!(
        run(home.path(), &["skills", "test", "--all"])
            .status
            .success()
    );
}

#[cfg(unix)]
mod gate {
    use super::*;
    use common::pty::{prompt, spawn};
    use expectrl::{Eof, Expect};

    const REVIEW: &str = r#"
[[rule]]
user = "reusable know-how"
times = 1
[[rule.tool_calls]]
name = "skill_view"
arguments = { name = "foo" }

[[rule]]
user = "reusable know-how"
tool_result = "Say alpha, beta or gamma."
times = 1
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "patch", name = "foo", old_string = "Say alpha, beta or gamma.", new_string = "Say alpha." }

[[rule]]
user = "reusable know-how"
text = "review done"

[[rule]]
repeat = true
text = "ok"
"#;

    fn foo(home: &Path) {
        let dir = skill_dir(home, "foo");
        write(
            &dir,
            "SKILL.md",
            "---\nname: foo\ndescription: Say words.\n---\nSay alpha, beta or gamma.\n",
        );
        let mut cases = Vec::new();
        for word in ["alpha", "beta", "gamma"] {
            cases.push(format!(
                "[[case]]\nname = \"{word}\"\nprompt = \"say {word}\"\nexpect_output_contains = \"{word}\"\nmock_script = \"mock/{word}.toml\"\n"
            ));
            write(
                &dir,
                &format!("evals/mock/{word}.toml"),
                &loads_then_says("foo", word),
            );
        }
        write(&dir, "evals/evals.toml", &cases.join("\n"));
    }

    /// Scenario: Regressing agent edit is rolled back — a background
    /// review's patch that drops foo from 3 of 3 to 1 of 3 passing cases
    /// is undone, and the rejection is in the review's transcript.
    #[test]
    fn a_regressing_background_review_patch_is_rolled_back() {
        let home = mock_home();
        foo(home.path());
        append_config(
            home.path(),
            "\n[skills]\neval_gate = true\neval_gate_repeat = 1\n",
        );
        let script = home.path().join("review.toml");
        std::fs::write(&script, REVIEW).unwrap();
        let mut p = spawn(
            home.path(),
            &["--no-color", "--mock-script", &script.to_string_lossy()],
        );
        prompt(&mut p);
        p.send_line("hello").unwrap();
        p.expect("ok").unwrap();
        prompt(&mut p);
        p.send_line("/refine tighten foo").unwrap();
        p.expect("review done").unwrap();
        p.send_line("/exit").unwrap();
        p.expect(Eof).unwrap();

        let kept = std::fs::read_to_string(skill_dir(home.path(), "foo").join("SKILL.md")).unwrap();
        assert!(kept.contains("Say alpha, beta or gamma."), "{kept}");
        // A background review writes no trajectory file of its own: its
        // stored transcript (the hidden child session) is the record.
        let listed = common::ok(&run(home.path(), &["--json", "sessions", "list", "--all"]));
        let listed: Value = serde_json::from_str(&listed).unwrap();
        let recorded = listed["sessions"].as_array().unwrap().iter().any(|s| {
            let id = s["id"].as_str().unwrap();
            stdout(&run(home.path(), &["sessions", "export", id]))
                .contains("edit rejected: eval pass rate fell from 3/3 to 1/3")
        });
        assert!(
            recorded,
            "the rejection is in the review's transcript: {listed}"
        );
    }
}

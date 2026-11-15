//! Evolution jobs end to end on the mock provider (spec:
//! harness-evolution, Isolated rollouts; Side-effect dry-run in rollouts;
//! Improver sessions; Proposals and adoption policy; Stale proposal
//! protection; User-driven selection; Job lifecycle, events and RPC;
//! Human-only evaluation of proposals).

#![cfg(feature = "evolve")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{append_config, mock_home, ok, run, stderr, stdout};
use serde_json::Value;

const MOCK: &str = include_str!("fixtures/evolve/mock.toml");

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A home with evolution on, the mock script, and a git workspace
/// holding `ledger.csv` (plus `files`, committed).
struct Setup {
    home: tempfile::TempDir,
    workspace: PathBuf,
}

impl Setup {
    fn new(config: &str, files: &[(&str, &str)]) -> Self {
        let home = mock_home();
        ok(&run(
            home.path(),
            &["config", "set", "evolve.enabled", "true"],
        ));
        std::fs::write(home.path().join("profiles/default/mock.toml"), MOCK).unwrap();
        append_config(
            home.path(),
            &format!("\n[providers.mock]\nscript = \"mock.toml\"\n{config}"),
        );
        let workspace = home.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("ledger.csv"), "amount\n10.00\n").unwrap();
        for (path, text) in files {
            let file = workspace.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        git(&workspace, &["init", "-q"]);
        git(&workspace, &["add", "-A"]);
        git(&workspace, &["commit", "-q", "-m", "init"]);
        Self { home, workspace }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    fn task(&self, statement: &str, extra: &str) -> PathBuf {
        let task = self
            .path()
            .join(format!("task-{}.toml", ulid::Ulid::generate()));
        std::fs::write(
            &task,
            format!(
                "statement = \"{statement}\"\ndomain = \"accounting\"\nworkspace = \"ws\"\n{extra}"
            ),
        )
        .unwrap();
        task
    }

    fn evolve(&self, task: &Path, extra: &[&str]) -> std::process::Output {
        let mut args = vec!["evolve", "run", "--task", task.to_str().unwrap()];
        args.extend_from_slice(extra);
        run(self.path(), &args)
    }

    fn only_job(&self) -> String {
        let jobs = json(self.path(), &["evolve", "list"]);
        jobs.as_array().unwrap().last().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn job_dir(&self, job: &str) -> PathBuf {
        self.path().join("profiles/default/evolve/jobs").join(job)
    }

    fn rollout(&self, job: &str, label: &str) -> Value {
        let path = if label.starts_with("base") {
            self.job_dir(job)
                .join("base")
                .join(label)
                .join("rollout.json")
        } else {
            self.job_dir(job)
                .join("modes/parallel")
                .join(label)
                .join("rollout.json")
        };
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn proposals(&self) -> Vec<Value> {
        json(self.path(), &["evolve", "proposals"])
            .as_array()
            .unwrap()
            .clone()
    }
}

/// Every file of `dir` with its bytes (`.git` included).
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// A foreground job: proposals, review, the improver's confinement, and
/// adoption (scenarios: Review before adoption; Improver cannot run
/// candidate scripts).
#[test]
fn a_foreground_job_proposes_a_reviewable_adoptable_harness() {
    let s = Setup::new("", &[]);
    let before = snapshot(&s.workspace);
    let out = s.evolve(
        &s.task("TASK-SUM the amounts in ledger.csv", ""),
        &["--k", "1"],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}{}", stderr(&out));
    assert!(text.contains("estimate: $"), "{text}");
    assert!(
        text.contains(" ok (") && text.contains("is ready: prometheus evolve review"),
        "{text}"
    );
    let job = s.only_job();
    let status = ok(&run(s.path(), &["evolve", "status", &job]));
    assert!(status.contains("selected A (agg1) by single"), "{status}");
    assert!(status.contains("spend: rollouts $"), "{status}");
    let proposals = s.proposals();
    assert_eq!(proposals[0]["status"], "pending");
    let id = proposals[0]["id"].as_str().unwrap().to_owned();
    let review = ok(&run(s.path(), &["evolve", "review", &id]));
    for part in [
        "+Always state the total with two decimals.",
        "census: ",
        "selector reason: ",
        "lesson: ",
    ] {
        assert!(review.contains(part), "{part} in {review}");
    }
    // The improver tried to run a candidate's script: no such tool.
    let sessions = json(s.path(), &["sessions", "list", "--all"]);
    let improver = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["source_tag"] == "evolve-improver")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let export = ok(&run(s.path(), &["sessions", "export", &improver]));
    assert!(
        export.contains("unknown tool") || export.contains("not available"),
        "{export}"
    );
    assert!(!export.contains("\"tool_name\":\"terminal\",\"metadata\":{\"is_error\":false"));
    // Rollouts and improvers are hidden from the session list.
    let visible = json(s.path(), &["sessions", "list"]);
    assert_eq!(
        visible["sessions"].as_array().unwrap().len(),
        0,
        "{visible}"
    );
    let adopted = ok(&run(s.path(), &["evolve", "adopt", &id]));
    assert!(
        adopted.contains("takes effect from the next session"),
        "{adopted}"
    );
    let log = json(s.path(), &["harness", "log"]);
    let active = log
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["active"] == true)
        .unwrap();
    assert_eq!(active["version"]["source"], "evolve");
    assert_eq!(active["version"]["job_id"], job.as_str());
    assert_eq!(before, snapshot(&s.workspace), "the workspace is untouched");
}

/// Scenarios: Approval inside a rollout; Workspace untouched (with a
/// rubric tracked at HEAD left out of the rollout workspace).
#[test]
fn rollouts_deny_approvals_and_work_on_a_copy_without_leak_files() {
    let s = Setup::new("", &[("grader/rubric.md", "full marks for 10.00")]);
    let before = snapshot(&s.workspace);
    let out = s.evolve(&s.task("TASK-APPROVAL sum the ledger", ""), &["--k", "1"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let job = s.only_job();
    let base = s.rollout(&job, "base1");
    let trajectory = base["entries"].to_string();
    assert!(trajectory.contains("BLOCKED"), "{trajectory}");
    assert_eq!(base["ok"], true, "the rollout went on after the denial");

    let out = s.evolve(&s.task("TASK-DELETE sum the ledger", ""), &["--k", "1"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let job = s.only_job();
    let base = s.rollout(&job, "base1");
    let listing = base["entries"].to_string();
    assert!(
        !listing.contains("rubric"),
        "the leak file is not in the rollout workspace: {listing}"
    );
    assert!(
        base["diff"].as_str().unwrap().contains("--- a/ledger.csv"),
        "{base}"
    );
    assert_eq!(
        before,
        snapshot(&s.workspace),
        "the user's workspace is byte-identical"
    );
}

/// Scenario: Message not sent.
#[test]
fn a_rollout_records_send_message_instead_of_sending_it() {
    let s = Setup::new("", &[]);
    let task = s.task(
        "TASK-MESSAGE report the total",
        "toolsets = [\"messaging\", \"file\"]\n",
    );
    let out = s.evolve(&task, &["--k", "1"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    let base = s.rollout(&s.only_job(), "base1");
    let calls = base["external_calls"].as_array().unwrap();
    assert_eq!(calls[0]["tool"], "send_message", "{base}");
    assert_eq!(calls[0]["arguments"]["target"], "telegram:42");
    assert!(
        base["entries"]
            .to_string()
            .contains("recorded, not executed during harness evolution")
    );
}

/// Scenario: Manual edit after job.
#[test]
fn adopting_a_stale_proposal_exits_65_and_keeps_the_active_version() {
    let s = Setup::new("", &[]);
    let out = s.evolve(&s.task("TASK-SUM the ledger", ""), &["--k", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let proposal = s.proposals()[0].clone();
    let manual = s.path().join("manual-harness");
    std::fs::create_dir_all(&manual).unwrap();
    for file in [
        "SYSTEM.md",
        "GUARDRAILS.md",
        "MEMORY_POLICY.md",
        "TOOL_GUIDANCE.md",
    ] {
        std::fs::write(manual.join(file), "").unwrap();
    }
    std::fs::write(manual.join("SYSTEM.md"), "Be brief.\n").unwrap();
    ok(&run(
        s.path(),
        &["harness", "apply", manual.to_str().unwrap()],
    ));
    let active = |s: &Setup| {
        json(s.path(), &["harness", "log"])
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["active"] == true)
            .unwrap()["version"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let h_now = active(&s);
    let out = run(
        s.path(),
        &["evolve", "adopt", proposal["id"].as_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    let expected = format!(
        "active harness changed since this proposal was produced (base {}, active {h_now}); re-run the job or adopt with --rebase",
        proposal["base_version"].as_str().unwrap()
    );
    assert!(stderr(&out).contains(&expected), "{}", stderr(&out));
    assert_eq!(active(&s), h_now);
    let rebased = ok(&run(
        s.path(),
        &[
            "evolve",
            "adopt",
            proposal["id"].as_str().unwrap(),
            "--rebase",
        ],
    ));
    assert!(rebased.contains("adopted"), "{rebased}");
}

/// Scenario: User picks a candidate.
#[test]
fn a_user_selection_pauses_the_job_until_choose() {
    let s = Setup::new("", &[]);
    ok(&run(
        s.path(),
        &["config", "set", "evolve.selection", "user"],
    ));
    let out = s.evolve(&s.task("TASK-SUM the ledger", ""), &["--k", "2"]);
    assert_eq!(out.status.code(), Some(1), "not ok yet: {}", stdout(&out));
    assert!(
        stdout(&out).contains("awaits your choice"),
        "{}",
        stdout(&out)
    );
    let job = s.only_job();
    let compare = ok(&run(s.path(), &["evolve", "compare", &job]));
    assert!(
        compare.contains("# Candidate A") && compare.contains("# Candidate B"),
        "{compare}"
    );
    assert!(!compare.contains("agg1"), "anonymous: {compare}");
    let chosen = run(s.path(), &["evolve", "choose", &job, "B"]);
    assert_eq!(
        chosen.status.code(),
        Some(0),
        "{}{}",
        stdout(&chosen),
        stderr(&chosen)
    );
    let status = json(s.path(), &["evolve", "status", &job]);
    let selected = &status["modes"][0]["selected"];
    assert_eq!(selected["id"], "B");
    assert_eq!(selected["selected_by"], "user");
    let mapped = status["modes"][0]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c[1] == "B")
        .unwrap()[0]
        .clone();
    assert_eq!(selected["label"], mapped);
    assert_eq!(s.proposals()[0]["label"], mapped);
}

#[test]
fn a_queued_job_can_be_cancelled() {
    let s = Setup::new("", &[]);
    let out = s.evolve(&s.task("TASK-SUM the ledger", ""), &["--detach"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let job = s.only_job();
    assert!(ok(&run(s.path(), &["evolve", "cancel", &job])).contains("cancelled"));
    assert_eq!(
        json(s.path(), &["evolve", "status", &job])["status"],
        "cancelled"
    );
    let again = run(s.path(), &["evolve", "cancel", &job]);
    assert_eq!(again.status.code(), Some(65), "{}", stderr(&again));
}

/// Scenario: Verdict does not reach the library.
#[test]
fn eval_results_reach_the_user_only() {
    let s = Setup::new("", &[]);
    let out = s.evolve(&s.task("TASK-SUM the ledger", ""), &["--k", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let id = s.proposals()[0]["id"].as_str().unwrap().to_owned();
    let review = ok(&run(
        s.path(),
        &["evolve", "review", &id, "--eval", "smoke"],
    ));
    assert!(review.contains("eval smoke (for you only"), "{review}");
    assert!(
        review.contains("base:") && review.contains("proposed:"),
        "{review}"
    );
    let results = s.path().join("profiles/default/eval-results");
    let verdicts: Vec<_> = walk(&results)
        .into_iter()
        .filter(|p| p.ends_with("verdict.json"))
        .collect();
    assert_eq!(verdicts.len(), 1);
    assert!(
        std::fs::read_to_string(&verdicts[0])
            .unwrap()
            .contains("pmt-eval-verdict")
    );
    ok(&run(s.path(), &["evolve", "adopt", &id]));
    let out = s.evolve(&s.task("TASK-SUM the ledger again", ""), &["--k", "1"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let status = json(s.path(), &["evolve", "status", &s.only_job()]);
    for stage in status["modes"][0]["stages"].as_array().unwrap() {
        assert_eq!(stage["fell_back"], false, "no tripwire hit: {stage}");
    }
    for file in walk(&s.path().join("profiles/default/evolve")) {
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("pmt-eval-verdict"),
            "{}",
            file.display()
        );
    }
    let unknown = run(s.path(), &["evolve", "review", &id, "--eval", "swe-bench"]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

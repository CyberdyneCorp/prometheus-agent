//! Task 60.4: the automation surfaces of one running daemon (spec:
//! api-server, OpenAI-compatible endpoints; Run idempotency ledger;
//! session-automation, Goal judge loop; mixture-of-agents, One-shot /moa
//! command; git-worktrees, Subagent worktree isolation): the official
//! OpenAI SDK chats with the profile and retries a run under one
//! idempotency key without running it twice; a `/goal` reaches completion
//! on the mock judge's word; `/moa` answers through a preset of mock
//! models; and a delegated child commits in its own git worktree.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use expectrl::Expect;
use serde_json::Value;

use super::openai_sdk;
use crate::common::{api, append_config, command, daemon, mock_home, ok, pty, run, stderr};

/// The goal judge's prompt starts with this; no other request does.
const JUDGE: &str = "You are the goal judge";

/// One script for every model call of the daemon's sessions.
fn rules(work: &Path) -> String {
    format!(
        r#"
[[rule]]
model = "ref-a"
repeat = true
text = "advice A"

[[rule]]
model = "ref-b"
repeat = true
text = "advice B"

[[rule]]
model = "agg"
repeat = true
text = "answered by the mixture"

[[rule]]
user = "{JUDGE}"
times = 1
text = '{{"verdict":"continue","reason":"the changelog is still missing"}}'

[[rule]]
user = "{JUDGE}"
repeat = true
text = '{{"verdict":"done","reason":"release notes and changelog written"}}'

[[rule]]
user = "hello from the sdk"
text = "hello back to the sdk"

[[rule]]
user = "stream to the sdk"
text = "streamed back in several words"

[[rule]]
user = "ticket work"
times = 1
text = "ticket done once"

[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = {{ tasks = [{{ goal = "Commit the fix", toolsets = ["terminal"] }}] }}

[[rule]]
user_regex = "Commit the fix"
tool_result_regex = "fix the bug"
text = "committed"

[[rule]]
user_regex = "Commit the fix"
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "echo fixed > fix.txt && git add fix.txt && git commit -q -m 'fix the bug' && git log --oneline -1" }}

[[rule]]
user = "please delegate"
tool_result_regex = "subagent_id"
text = "the child committed in {work}"

[[rule]]
repeat = true
text = "worked on it"
"#,
        work = work.display()
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn repo(home: &Path) -> PathBuf {
    let work = home.canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    work
}

/// A mock home serving the API, with MoA presets on mock models,
/// worktree isolation for children and the script above.
fn automation_home() -> (tempfile::TempDir, PathBuf) {
    let home = mock_home();
    let work = repo(home.path());
    std::fs::write(
        home.path().join("profiles/default/script.toml"),
        rules(&work),
    )
    .unwrap();
    append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"script.toml\"\n\n\
         [delegation]\nworktree_isolation = true\n\n\
         [moa.presets.default]\n\
         reference_models = [{provider = \"mock\", model = \"ref-a\"}, {provider = \"mock\", model = \"ref-b\"}]\n\
         aggregator = {provider = \"mock\", model = \"agg\"}\n",
    );
    api::enable(home.path(), "default", "");
    api::listen(home.path(), "");
    (home, work)
}

fn sdk_part(port: u16) {
    let report = openai_sdk::drive(
        &format!("http://127.0.0.1:{port}/v1"),
        &api::key_of("default"),
    );
    assert_eq!(report["sdk"], openai_sdk::VERSION);
    assert_eq!(report["models"][0], "prometheus-agent", "{report}");
    assert_eq!(report["chat"]["object"], "chat.completion");
    assert_eq!(report["chat"]["content"], "hello back to the sdk");
    assert_eq!(report["chat"]["finish_reason"], "stop");
    assert_eq!(
        report["streamed"]["content"],
        "streamed back in several words"
    );
    assert!(
        report["streamed"]["chunks"].as_u64().unwrap() > 1,
        "{report}"
    );
    let runs = &report["runs"];
    assert_eq!(runs["first"]["status"], 202, "{report}");
    assert_eq!(runs["retry"]["status"], 202, "{report}");
    assert_eq!(runs["retry"]["run_id"], runs["first"]["run_id"]);
    assert_eq!(runs["retry"]["replayed"], "true");
    // The script answers `ticket work` once: a second execution would
    // have said `worked on it`.
    assert_eq!(runs["final"]["status"], "completed", "{report}");
    assert_eq!(runs["final"]["output"], "ticket done once");
}

fn line_chat_part(home: &Path) {
    let mut p = pty::spawn(home, &["--no-color"]);
    p.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut p);
    p.send_line("/goal write the release notes").unwrap();
    p.expect("Goal set: write the release notes").unwrap();
    p.expect("Continuing toward goal (1/20): the changelog is still missing")
        .unwrap();
    p.expect("Goal achieved: release notes and changelog written")
        .unwrap();
    pty::prompt(&mut p);
    p.send_line("/moa design a migration plan").unwrap();
    p.expect("answered by the mixture").unwrap();
    pty::prompt(&mut p);
    pty::exit(p);
}

fn worktree_part(home: &Path, work: &Path) {
    let out = command(home)
        .current_dir(work)
        .args(["--json", "-z", "please delegate"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let branches = git(work, &["branch", "--list", "prometheus/subagent-*"]);
    let branch = branches.trim_start_matches(['*', ' ']).trim();
    assert!(branch.starts_with("prometheus/subagent-"), "{branches}");
    assert_eq!(
        git(work, &["log", "-1", "--format=%s", branch]),
        "fix the bug"
    );
    // The main checkout is untouched and the child's tree is gone.
    assert_eq!(git(work, &["status", "--porcelain"]), "");
    assert!(!work.join("fix.txt").exists());
    let id = branch.trim_start_matches("prometheus/");
    assert!(!work.join(".worktrees").join(id).exists());
}

#[cfg(unix)]
#[test]
fn sdk_runs_goals_mixtures_and_worktree_children_share_one_daemon() {
    let (home, work) = automation_home();
    let _daemon = daemon::start(home.path(), &[]);
    let port = api::port(home.path());
    sdk_part(port);
    line_chat_part(home.path());
    worktree_part(home.path(), &work);
    // Every surface's session belongs to the one profile the daemon serves.
    let listed: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "list", "--all"],
    )))
    .unwrap();
    let sources: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["source"].as_str())
        .collect();
    for source in ["api", "subagent"] {
        assert!(sources.contains(&source), "{sources:?}");
    }
}

//! `prometheus chat` without a terminal (spec: cli, Top-level command
//! tree; Chat flags; Stable exit codes; One-shot and piped mode; Interface
//! selection is TTY-first; Resume and continue resolution; Seed prompt and
//! prompt file), against the built binary on a temp home with the mock
//! provider.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{command, ok, run, run_with_stdin, stderr, stdout, temp_home};
use serde_json::Value;

/// A home whose default profile talks to the mock provider.
fn mock_home() -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "mock"],
    ));
    home
}

/// Writes a mock script under the home and returns its path.
fn script(home: &Path, text: &str) -> PathBuf {
    let path = home.join("script.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// A directory under the home (created, canonical).
fn dir(home: &Path, rel: &str) -> PathBuf {
    let path = home.join(rel);
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

/// `-z <prompt> --json` from `cwd`, with `extra` flags.
fn oneshot_in(home: &Path, cwd: &Path, prompt: &str, extra: &[&str]) -> Output {
    command(home)
        .current_dir(cwd)
        .args(["--json", "-z", prompt])
        .args(extra)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    serde_json::from_str(stdout(out).trim())
        .unwrap_or_else(|e| panic!("{e}: stdout {} stderr {}", stdout(out), stderr(out)))
}

fn session_of(out: &Output) -> String {
    json(out)["session_id"].as_str().unwrap().to_owned()
}

/// The 19.2 verification: `echo hi | prometheus chat --provider mock`.
#[test]
fn a_piped_prompt_prints_only_the_answer() {
    let home = temp_home();
    let out = run_with_stdin(home.path(), &["chat", "--provider", "mock"], "hi\n");
    assert_eq!(ok(&out), "mock: hi\n");
    // Scenario: Piped prompt (`cat notes.md | prometheus -z summarise`):
    // stdout is the answer only, the piped text follows the flag's prompt.
    let home = mock_home();
    let out = run_with_stdin(home.path(), &["-z", "summarise"], "line one\n");
    assert_eq!(ok(&out), "mock: summarise\n\nline one\n");
}

/// Scenario: Missing provider configuration.
#[test]
fn a_fresh_profile_without_a_model_exits_78() {
    let home = temp_home();
    let out = run(home.path(), &["-z", "hi"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("no model configured; run: prometheus setup model"));
    assert!(stdout(&out).is_empty());
}

/// Scenario: Config cannot force TUI into a pipe.
#[test]
fn a_pipe_runs_one_shot_whatever_the_interface_setting() {
    let home = mock_home();
    ok(&run(home.path(), &["config", "set", "ui.interface", "tui"]));
    let out = run_with_stdin(home.path(), &[], "hi");
    assert_eq!(ok(&out), "mock: hi\n");
    let out = run_with_stdin(home.path(), &["chat", "--tui"], "hi");
    assert_eq!(ok(&out), "mock: hi\n");
}

#[test]
fn json_reports_session_answer_usage_and_exit_reason() {
    let home = mock_home();
    let out = oneshot_in(home.path(), home.path(), "hi", &[]);
    let body = json(&out);
    assert_eq!(body["answer"], "mock: hi");
    assert_eq!(body["exit_reason"], "completed");
    assert!(body["usage"]["calls"].as_u64().unwrap() >= 1);
    // One-shot sessions are tagged so pickers and `-c` skip them.
    let listed = ok(&run(home.path(), &["sessions", "list", "--all"]));
    assert!(listed.contains("(oneshot)"), "{listed}");
}

/// Scenario: Approval in unattended run.
#[test]
fn an_approval_in_a_one_shot_run_ends_it_with_exit_4() {
    let home = mock_home();
    let path = script(
        home.path(),
        r#"
[[rule]]
user = "clean"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "git reset --hard" }

[[rule]]
tool_result = "BLOCKED"
text = "could not"
"#,
    );
    let mock = ["--mock-script", path.to_str().unwrap()];
    let out = oneshot_in(home.path(), home.path(), "clean up", &mock);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("chat.approval_required"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Untrusted body from a file.
#[test]
fn a_prompt_file_is_sent_verbatim_and_never_run() {
    let home = mock_home();
    let canary = dir(home.path(), "user-home/keep");
    let body = home.path().join("body.txt");
    std::fs::write(&body, "!rm -rf ~").unwrap();
    let out = run(
        home.path(),
        &["-z", "--prompt-file", body.to_str().unwrap()],
    );
    assert_eq!(ok(&out), "mock: !rm -rf ~\n");
    assert!(canary.exists());
}

#[test]
fn prompt_sources_are_exclusive_and_a_missing_file_exits_66() {
    let home = mock_home();
    let out = run(home.path(), &["-z", "a", "--prompt-file", "b.txt"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let out = run(home.path(), &["--query", "a", "-z"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let out = run(home.path(), &["-z", "--prompt-file", "missing.txt"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
}

/// Scenario: Unknown subcommand.
#[test]
fn a_mistyped_subcommand_exits_2_with_a_suggestion() {
    let home = temp_home();
    let out = run(home.path(), &["sesions"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("unknown command 'sesions' (did you mean 'sessions'?)"),
        "{}",
        stderr(&out)
    );
}

/// The global flags parse before and after the subcommand.
#[test]
fn global_flags_are_accepted_on_either_side() {
    let home = mock_home();
    for args in [
        &["--no-color", "-q", "chat", "-z", "hi"][..],
        &["chat", "-z", "hi", "--no-color", "-q", "-v"],
    ] {
        assert_eq!(ok(&run(home.path(), args)), "mock: hi\n", "{args:?}");
    }
    // Chat flags belong to chat.
    let out = run(home.path(), &["-m", "mock", "sessions", "list"]);
    assert_eq!(out.status.code(), Some(2));
}

/// A run without a final answer because a budget ran out exits 3.
#[test]
fn a_one_shot_run_out_of_iterations_exits_3() {
    let home = mock_home();
    let path = script(
        home.path(),
        r#"
[[rule]]
repeat = true
[[rule.tool_calls]]
name = "todo"
arguments = { todos = [] }
"#,
    );
    let mock = [
        "--mock-script",
        path.to_str().unwrap(),
        "--max-iterations",
        "1",
    ];
    let out = oneshot_in(home.path(), home.path(), "loop", &mock);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
}

/// Scenario: Continue in current directory.
#[test]
fn continue_resumes_the_newest_cli_session_of_this_directory() {
    let home = mock_home();
    let (a, b) = (dir(home.path(), "repo/a"), dir(home.path(), "repo/b"));
    // `--source` keeps the sessions visible to `-c` (one-shot sessions are
    // tagged `oneshot` and skipped).
    let tag = ["--source", "tests"];
    let in_b = session_of(&oneshot_in(home.path(), &b, "older", &tag));
    let in_a = session_of(&oneshot_in(home.path(), &a, "newer", &tag));
    let resumed = session_of(&oneshot_in(home.path(), &b, "again", &["-c"]));
    assert_eq!(resumed, in_b);
    assert_ne!(resumed, in_a);
    let nothing = dir(home.path(), "repo/empty");
    let out = oneshot_in(home.path(), &nothing, "x", &["-c"]);
    assert_eq!(out.status.code(), Some(66));
    assert!(stderr(&out).contains(&format!("no session to continue in {}", nothing.display())));
}

/// Scenario: Continue by name picks newest numbered session.
#[test]
fn continue_by_name_picks_the_newest_numbered_session() {
    let home = mock_home();
    let repo = dir(home.path(), "repo");
    let tag = ["--source", "tests"];
    let first = session_of(&oneshot_in(home.path(), &repo, "one", &tag));
    let third = session_of(&oneshot_in(home.path(), &repo, "two", &tag));
    ok(&run(
        home.path(),
        &["sessions", "rename", &first, "Refactor"],
    ));
    ok(&run(
        home.path(),
        &["sessions", "rename", &third, "Refactor #3"],
    ));
    let resumed = session_of(&oneshot_in(
        home.path(),
        &repo,
        "go on",
        &["-c", "Refactor"],
    ));
    assert_eq!(resumed, third);
}

/// Scenario: Explicit cwd overrides restore (and without `--cwd` the
/// recorded directory is restored).
#[test]
fn an_explicit_cwd_overrides_the_restored_one() {
    let home = mock_home();
    let (a, b) = (dir(home.path(), "repo/a"), dir(home.path(), "repo/b"));
    std::fs::write(a.join("marker.txt"), "A-side").unwrap();
    std::fs::write(b.join("marker.txt"), "B-side").unwrap();
    // The tool-result rules come first: after the read, the newest tool
    // result names the side the session ran in.
    let path = script(
        home.path(),
        r#"
[[rule]]
tool_result = "B-side"
repeat = true
text = "in b"

[[rule]]
tool_result = "A-side"
repeat = true
text = "in a"

[[rule]]
user = "where"
repeat = true
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "marker.txt" }

[[rule]]
repeat = true
text = "started"
"#,
    );
    let mock = ["--mock-script", path.to_str().unwrap()];
    let elsewhere = dir(home.path(), "elsewhere");
    let session = session_of(&oneshot_in(home.path(), &a, "start", &mock));
    let restored = oneshot_in(
        home.path(),
        &elsewhere,
        "where",
        &[&mock[..], &["-r", &session]].concat(),
    );
    assert_eq!(json(&restored)["answer"], "in a", "{}", stderr(&restored));
    assert!(stderr(&restored).contains(&format!("cwd restored to {}", a.display())));
    let session = session_of(&oneshot_in(home.path(), &a, "start", &mock));
    let b_flag = b.to_str().unwrap();
    let args = [&mock[..], &["--resume", &session, "--cwd", b_flag]].concat();
    let moved = oneshot_in(home.path(), &elsewhere, "where", &args);
    assert_eq!(json(&moved)["answer"], "in b", "{}", stderr(&moved));
    assert!(!stderr(&moved).contains("cwd restored"));
}

#[test]
fn resume_latest_and_unknown_references() {
    let home = mock_home();
    let repo = dir(home.path(), "repo");
    let tag = ["--source", "tests"];
    let made = session_of(&oneshot_in(home.path(), &repo, "one", &tag));
    let latest = session_of(&oneshot_in(home.path(), &repo, "two", &["-r", "latest"]));
    assert_eq!(latest, made);
    let out = oneshot_in(home.path(), &repo, "x", &["-r", "ses_NOPE"]);
    assert_eq!(out.status.code(), Some(66));
    let out = oneshot_in(home.path(), &repo, "x", &["-r", "@codex"]);
    assert_eq!(out.status.code(), Some(69));
}

#[cfg(unix)]
#[test]
fn sigint_stops_a_one_shot_run_with_exit_130() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let home = mock_home();
    let path = script(
        home.path(),
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"slow\"\n",
    );
    let mut child = command(home.path())
        .args(["-z", "hi", "--mock-script", path.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
    kill(pid, Signal::SIGINT).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
}

/// Compares `actual` with `tests/snapshots/<name>` (`UPDATE_SNAPSHOTS=1`
/// rewrites it).
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

/// The command tree and the chat flags as `--help` prints them.
#[test]
fn help_matches_its_snapshots() {
    let home = temp_home();
    snapshot("cli-help.txt", &ok(&run(home.path(), &["--help"])));
    snapshot("chat-help.txt", &ok(&run(home.path(), &["chat", "--help"])));
}

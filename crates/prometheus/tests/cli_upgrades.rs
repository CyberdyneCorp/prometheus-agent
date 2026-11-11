//! M2 CLI upgrades of the chat (spec: cli, Streaming JSONL output;
//! Planning-only turn; Exit with delete and profile archive slash forms),
//! against the built binary on a temp home with the mock provider.

mod common;

use std::path::Path;
use std::process::Output;

use common::{command, mock_home, ok, run, stderr, stdout};
use serde_json::Value;

/// Writes a mock script under the home and returns its path.
fn script(home: &Path, text: &str) -> String {
    let path = home.join("script.toml");
    std::fs::write(&path, text).unwrap();
    path.to_string_lossy().into_owned()
}

/// A model that lists the directory once, then answers.
const LIST_FILES: &str = r#"
[[rule]]
user = "list files"
[[rule.tool_calls]]
name = "search_files"
arguments = { pattern = "*.txt", target = "files" }

[[rule]]
tool_result = "notes.txt"
text = "one file: notes.txt"
"#;

/// Every stdout line as JSON (panics on a line that is not).
fn json_lines(out: &Output) -> Vec<Value> {
    stdout(out)
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line:?}\n{}", stderr(out)))
        })
        .collect()
}

fn types(lines: &[Value]) -> Vec<&str> {
    lines.iter().filter_map(|l| l["type"].as_str()).collect()
}

/// Asserts the stream of a run that listed the files: trajectory events in
/// order, then exactly one closing `result` line.
fn assert_streamed_listing(lines: &[Value]) {
    let kinds = types(lines);
    for kind in [
        "session.start",
        "tool.call",
        "tool.result",
        "assistant.message",
        "turn.end",
    ] {
        assert!(kinds.contains(&kind), "{kind} missing: {kinds:?}");
    }
    let position = |kind: &str| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position("tool.call") < position("tool.result"));
    assert!(position("tool.result") < position("turn.end"));
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "result", "{last}");
    assert_eq!(last["exit_reason"], "completed");
    assert_eq!(last["answer"], "one file: notes.txt");
    assert!(
        last["session_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("ses_"))
    );
    assert!(last["usage"].is_object(), "{last}");
    assert_eq!(kinds.iter().filter(|k| **k == "result").count(), 1);
}

/// Scenario: Stream ends with result. Every stdout line parses as JSON in
/// the trajectory's line shape, and the last one is the `result`.
#[test]
fn a_streamed_one_shot_run_writes_json_lines_ending_with_the_result() {
    let home = mock_home();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.txt"), "x\n").unwrap();
    let mock = script(home.path(), LIST_FILES);
    let out = command(home.path())
        .current_dir(&work)
        .args(["-z", "list files", "--output-format", "stream-json"])
        .args(["--mock-script", &mock])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let lines = json_lines(&out);
    assert_streamed_listing(&lines);
    let start = &lines[0];
    assert_eq!(start["type"], "session.start");
    assert_eq!(start["seq"], 1);
    assert!(start["v"].is_string() && start["ts"].is_string(), "{start}");
    // `--output-format json` is `--json`: one document.
    let out = command(home.path())
        .current_dir(&work)
        .args(["-z", "list files", "--output-format", "json"])
        .args(["--mock-script", &mock])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let lines = json_lines(&out);
    assert_eq!(lines.len(), 1, "{}", stdout(&out));
    assert_eq!(lines[0]["exit_reason"], "completed");
}

/// The documented exit codes are kept: a run stopped on an approval
/// still exits 4, and its stream still ends with a `result` line.
#[test]
fn a_streamed_run_keeps_the_exit_codes_and_ends_with_the_result() {
    let home = mock_home();
    let mock = script(
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
    let out = command(home.path())
        .current_dir(home.path())
        .args(["-z", "clean up", "--output-format", "stream-json"])
        .args(["--mock-script", &mock])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let lines = json_lines(&out);
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["exit_reason"], "approval_required");
}

/// The stream of a run attached to the home's daemon has the same shape.
#[cfg(feature = "gateway")]
#[test]
fn a_streamed_run_attached_to_the_daemon_ends_with_the_result() {
    let home = mock_home();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.txt"), "x\n").unwrap();
    let mock = script(home.path(), LIST_FILES);
    let _daemon = common::daemon::start(home.path(), &[]);
    let out = command(home.path())
        .current_dir(&work)
        .args(["-z", "list files", "--output-format", "stream-json"])
        .args(["--mock-script", &mock])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_streamed_listing(&json_lines(&out));
}

#[cfg(unix)]
mod line {
    use expectrl::{Eof, Expect as _};

    use super::common::pty::{prompt, spawn};
    use super::common::{newest_session, session_messages};
    use super::*;

    /// An interactive session cannot stream: exit 2 before anything runs.
    #[test]
    fn stream_json_is_refused_for_an_interactive_session() {
        let home = mock_home();
        for interface in [&["--line"][..], &["--tui"]] {
            let mut args = vec!["--output-format", "stream-json"];
            args.extend_from_slice(interface);
            let mut p = spawn(home.path(), &args);
            p.expect("one-shot runs").unwrap();
            p.expect(Eof).unwrap();
            let status = p.get_process_mut().wait().unwrap();
            assert!(format!("{status:?}").ends_with(", 2)"), "{status:?}");
        }
    }

    /// Scenario: Write outside plans refused. During `/plan add caching`
    /// the model's `write_file` of `src/main.rs` is refused with `not
    /// allowed in /plan`; only the plan file is written, and its path is
    /// printed at the end. The next turn is unrestricted again.
    #[test]
    fn a_plan_turn_writes_only_its_plan_file() {
        let home = mock_home();
        let work = home.path().join("work");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mock = script(
            home.path(),
            r##"
[[rule]]
user = "Plan this task: add caching"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "src/main.rs", content = "changed\n" }
[[rule.tool_calls]]
name = "write_file"
arguments = { path = ".prometheus/plans/add-caching.md", content = "# Add caching\n" }

[[rule]]
tool_result = "not allowed in /plan"
text = "planned"

[[rule]]
user = "now edit"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "src/lib.rs", content = "pub fn cached() {}\n" }

[[rule]]
tool_result = "lib.rs"
text = "edited"
"##,
        );
        let cwd = work.to_str().unwrap();
        let mut p = spawn(home.path(), &["--mock-script", &mock, "--cwd", cwd]);
        prompt(&mut p);
        p.send_line("/plan add caching").unwrap();
        p.expect("planned").unwrap();
        p.expect("plan written to").unwrap();
        p.expect("add-caching.md").unwrap();
        prompt(&mut p);
        let read = |rel: &str| std::fs::read_to_string(work.join(rel)).ok();
        assert_eq!(read("src/main.rs").as_deref(), Some("fn main() {}\n"));
        assert_eq!(
            read(".prometheus/plans/add-caching.md").as_deref(),
            Some("# Add caching\n")
        );
        let refused = session_messages(home.path(), &newest_session(home.path()))
            .iter()
            .any(|m| m.to_string().contains("not allowed in /plan"));
        assert!(refused, "the refusal reaches the model");
        // The restriction lasted one turn.
        p.send_line("now edit").unwrap();
        p.expect("edited").unwrap();
        prompt(&mut p);
        assert_eq!(read("src/lib.rs").as_deref(), Some("pub fn cached() {}\n"));
        // A second plan of the same task gets its own file.
        p.send_line("/exit").unwrap();
        p.expect(Eof).unwrap();
        let plan = home.path().join("work/.prometheus/plans");
        let mut p = spawn(home.path(), &["--mock-script", &mock, "--cwd", cwd]);
        prompt(&mut p);
        p.send_line("/plan add caching").unwrap();
        p.expect("no plan was written").unwrap();
        p.expect("add-caching-2.md").unwrap();
        prompt(&mut p);
        super::common::pty::exit(p);
        assert!(plan.join("add-caching.md").is_file());
    }

    /// Scenario: Exit and delete. `/exit --delete --yes` exits 0 and the
    /// session is gone (`sessions show` exits 66); without `--yes` the
    /// destructive confirmation asks first, and `cancel` keeps it.
    #[test]
    fn exit_with_delete_removes_the_session() {
        let home = mock_home();
        let mut p = spawn(home.path(), &[]);
        prompt(&mut p);
        p.send_line("keep me").unwrap();
        p.expect("mock: keep me").unwrap();
        prompt(&mut p);
        let id = newest_session(home.path());
        p.send_line("/exit --delete").unwrap();
        p.expect("once / always / cancel").unwrap();
        p.send_line("cancel").unwrap();
        p.expect("cancelled").unwrap();
        prompt(&mut p);
        p.send_line("/exit --delete --yes").unwrap();
        p.expect(Eof).unwrap();
        let status = p.get_process_mut().wait().unwrap();
        assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
        let shown = run(home.path(), &["sessions", "show", &id]);
        assert_eq!(shown.status.code(), Some(66), "{}", stderr(&shown));
    }

    /// `/export` and `/import` map to `profile export` and `profile
    /// import` with their defaults: the copy is a new profile.
    #[test]
    fn export_and_import_slash_forms_copy_a_profile() {
        let home = mock_home();
        let mut p = spawn(home.path(), &[]);
        prompt(&mut p);
        p.send_line("/export -o saved.tar.zst").unwrap();
        p.expect("exported profile 'default'").unwrap();
        prompt(&mut p);
        assert!(home.path().join("saved.tar.zst").is_file());
        p.send_line("/import saved.tar.zst --name copy").unwrap();
        p.expect("as 'copy'").unwrap();
        prompt(&mut p);
        super::common::pty::exit(p);
        let listed = ok(&run(home.path(), &["profile", "list"]));
        assert!(listed.contains("copy"), "{listed}");
    }

    /// The daemon-attached line interface deletes on `/exit --delete` too.
    #[cfg(feature = "gateway")]
    #[test]
    fn exit_with_delete_removes_a_daemon_session() {
        let home = mock_home();
        let _daemon = super::common::daemon::start(home.path(), &[]);
        let mut p = spawn(home.path(), &[]);
        p.expect("attached to the gateway daemon").unwrap();
        prompt(&mut p);
        p.send_line("keep me").unwrap();
        p.expect("mock: keep me").unwrap();
        prompt(&mut p);
        let id = newest_session(home.path());
        p.send_line("/exit --delete --yes").unwrap();
        p.expect(Eof).unwrap();
        let status = p.get_process_mut().wait().unwrap();
        assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
        let shown = run(home.path(), &["sessions", "show", &id]);
        assert_eq!(shown.status.code(), Some(66), "{}", stderr(&shown));
    }
}

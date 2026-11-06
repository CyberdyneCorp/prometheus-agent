//! `prometheus chat -w`, the exit disposition and `prometheus worktrees`
//! against git fixture repositories on the mock provider (spec:
//! git-worktrees).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{command, mock_home, ok, stderr, stdout};
use serde_json::Value;

/// Runs git in `dir` without the user's configuration.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
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

/// A repository `work` (with a bare `origin`) inside `home`.
fn repo(home: &Path) -> PathBuf {
    let base = home.canonicalize().unwrap();
    let origin = base.join("origin.git");
    let work = base.join("work");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(work.join("README.md"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "init"]);
    git(
        &work,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    );
    git(&work, &["push", "-q", "-u", "origin", "main"]);
    git(&work, &["remote", "set-head", "origin", "main"]);
    work
}

fn script(home: &Path, body: &str) -> PathBuf {
    let path = home.join("script.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// `pwd` through the terminal, answering whether it ran in a tree.
const PWD: &str = r#"
[[rule]]
user_regex = "(?s).*"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "pwd" }

[[rule]]
tool_result_regex = "\\.worktrees/"
text = "ran in a worktree"

[[rule]]
tool_result_regex = "(?s).*"
text = "ran elsewhere"
"#;

fn oneshot_in(home: &Path, dir: &Path, args: &[&str], prompt: &str) -> Output {
    let script = script(home, PWD);
    command(home)
        .current_dir(dir)
        .arg("--json")
        .args(args)
        .args(["-z", prompt, "--mock-script"])
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn answer(out: &Output) -> String {
    let body: Value = serde_json::from_str(stdout(out).trim())
        .unwrap_or_else(|e| panic!("{e}: {} {}", stdout(out), stderr(out)));
    body["answer"].as_str().unwrap_or_default().to_owned()
}

fn trees(work: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(work.join(".worktrees"))
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Scenario: One-shot in a worktree.
#[test]
fn a_one_shot_run_works_in_a_new_tree_and_leaves_the_checkout_unchanged() {
    let home = mock_home();
    let work = repo(home.path());
    let out = oneshot_in(home.path(), &work, &["-w"], "Fix issue #123");
    assert_eq!(answer(&out), "ran in a worktree", "{}", stderr(&out));
    let made = trees(&work);
    assert_eq!(made.len(), 1, "{made:?}");
    let name = made[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("prom-") && name.len() == 13, "{name}");
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    assert_eq!(git(&work, &["branch", "--show-current"]), "main");
    assert_eq!(
        git(&made[0], &["branch", "--show-current"]),
        format!("prometheus/{name}")
    );
    // A single-query run never prompts and keeps the tree for the sweep.
    assert!(
        stderr(&out).contains("worktree kept at"),
        "{}",
        stderr(&out)
    );
    let repos =
        std::fs::read_to_string(home.path().join("profiles/default/worktree_repos.json")).unwrap();
    assert!(repos.contains(&*work.to_string_lossy()), "{repos}");
}

#[test]
fn a_resumed_session_reopens_its_tree() {
    let home = mock_home();
    let work = repo(home.path());
    let first = oneshot_in(home.path(), &work, &["-w", "lane"], "first");
    assert_eq!(answer(&first), "ran in a worktree");
    let id = common::newest_session(home.path());
    let second = oneshot_in(home.path(), &work, &["-w", "-r", &id], "again");
    assert_eq!(answer(&second), "ran in a worktree", "{}", stderr(&second));
    assert_eq!(trees(&work), vec![work.join(".worktrees/lane")]);
}

#[test]
fn outside_a_repository_worktree_mode_exits_2() {
    let home = mock_home();
    let dir = home.path().canonicalize().unwrap().join("plain");
    std::fs::create_dir_all(&dir).unwrap();
    let script = script(home.path(), PWD);
    let out = command(home.path())
        .current_dir(&dir)
        // The test tree itself lives in this project's repository.
        .env(
            "GIT_CEILING_DIRECTORIES",
            home.path().canonicalize().unwrap(),
        )
        .args(["-w", "-z", "hi", "--mock-script"])
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--worktree requires a git repository"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_existing_name_is_refused() {
    let home = mock_home();
    let work = repo(home.path());
    ok(&oneshot_in(home.path(), &work, &["-w", "exp"], "one"));
    let out = oneshot_in(home.path(), &work, &["-w", "exp"], "two");
    assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("worktree 'exp' already exists"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_worktrees_cli_lists_prunes_and_refuses_unknown_archives() {
    let home = mock_home();
    let work = repo(home.path());
    ok(&oneshot_in(home.path(), &work, &["-w", "lane"], "one"));
    let listed = ok(&command(home.path())
        .current_dir(&work)
        .args(["--json", "worktrees", "list"])
        .output()
        .unwrap());
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let trees = &listed["repos"][0]["trees"];
    assert_eq!(trees[0]["name"], "lane", "{listed}");
    assert_eq!(trees[0]["branch"], "prometheus/lane");

    let dry = ok(&command(home.path())
        .current_dir(&work)
        .args(["worktrees", "prune", "--dry-run"])
        .output()
        .unwrap());
    assert!(dry.contains("lane  keep (recent-activity)"), "{dry}");
    assert!(work.join(".worktrees/lane").exists());

    let unknown = command(home.path())
        .current_dir(&work)
        .args(["worktrees", "restore", "nope"])
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(66), "{}", stderr(&unknown));
}

#[cfg(unix)]
mod interactive {
    use std::time::Duration;

    use expectrl::session::OsSession;
    use expectrl::{ControlCode, Eof, Expect};

    use super::*;

    fn line_chat(home: &Path, dir: &Path, args: &[&str]) -> OsSession {
        let script = script(home, "[[rule]]\nrepeat = true\ntext = \"ok\"\n");
        let mut cmd = command(home);
        cmd.args(common::pty::LINE)
            .args(args)
            .arg("--mock-script")
            .arg(&script)
            .current_dir(dir)
            .env("TERM", "xterm")
            .env_remove("NO_COLOR");
        let mut session = OsSession::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(Duration::from_secs(30)));
        session
    }

    fn finished(mut p: OsSession) {
        p.expect(Eof).unwrap();
        let status = p.get_process_mut().wait().unwrap();
        assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
    }

    /// Scenario: Ctrl-D with uncommitted work.
    #[test]
    fn enter_at_the_exit_prompt_keeps_a_tree_with_uncommitted_work() {
        let home = mock_home();
        let work = repo(home.path());
        let mut p = line_chat(home.path(), &work, &["-w", "wip"]);
        common::pty::prompt(&mut p);
        let tree = work.join(".worktrees/wip");
        std::fs::write(tree.join("draft.txt"), "unfinished").unwrap();
        p.send(ControlCode::EndOfTransmission).unwrap();
        p.expect("[k]eep (default), [a]rchive and remove, [r]emove and discard")
            .unwrap();
        p.send_line("").unwrap();
        p.expect("worktree kept at").unwrap();
        finished(p);
        assert_eq!(
            std::fs::read_to_string(tree.join("draft.txt")).unwrap(),
            "unfinished"
        );
        git(
            &work,
            &["rev-parse", "--verify", "refs/heads/prometheus/wip"],
        );
    }

    #[test]
    fn a_tree_with_nothing_to_lose_is_removed_without_asking() {
        let home = mock_home();
        let work = repo(home.path());
        let mut p = line_chat(home.path(), &work, &["-w", "clean"]);
        common::pty::prompt(&mut p);
        p.send(ControlCode::EndOfTransmission).unwrap();
        p.expect("removed").unwrap();
        finished(p);
        assert!(!work.join(".worktrees/clean").exists());
        assert_eq!(git(&work, &["branch", "--list", "prometheus/clean"]), "");
    }

    /// Scenario: Restore an archived lane.
    #[test]
    fn an_archived_lane_restores_with_its_staged_and_untracked_files() {
        let home = mock_home();
        let work = repo(home.path());
        let mut p = line_chat(home.path(), &work, &["-w", "lane-x"]);
        common::pty::prompt(&mut p);
        let tree = work.join(".worktrees/lane-x");
        std::fs::write(tree.join("staged.txt"), "staged").unwrap();
        git(&tree, &["add", "staged.txt"]);
        std::fs::write(tree.join("untracked.txt"), "untracked").unwrap();
        p.send(ControlCode::EndOfTransmission).unwrap();
        p.expect("[r]emove and discard").unwrap();
        p.send_line("a").unwrap();
        p.expect("prometheus worktrees restore lane-x").unwrap();
        finished(p);
        assert!(!tree.exists());

        let out = command(home.path())
            .current_dir(&work)
            .args(["worktrees", "restore", "lane-x"])
            .output()
            .unwrap();
        assert!(ok(&out).contains("restored lane-x"), "{}", stdout(&out));
        assert_eq!(
            git(&tree, &["diff", "--cached", "--name-only"]),
            "staged.txt"
        );
        assert_eq!(
            git(&tree, &["status", "--porcelain"]),
            "A  staged.txt\n?? untracked.txt"
        );
        assert_eq!(
            git(&tree, &["branch", "--show-current"]),
            "prometheus/lane-x"
        );
    }
}

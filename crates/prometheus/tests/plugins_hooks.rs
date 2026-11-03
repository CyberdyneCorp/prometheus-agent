//! Shell hooks, the hooks CLI and gateway hook directories (spec:
//! plugin-system, Shell hooks with first-use consent; Hooks CLI; Gateway
//! hook directories), with hook fixtures written by the tests.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{
    append_config, command, mock_home, newest_session, ok, run, session_messages, stderr, stdout,
};

/// Writes an executable script `name` under `<home>/hook-bin/`.
fn script(home: &Path, name: &str, body: &str) -> PathBuf {
    let dir = home.join("hook-bin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn work(home: &Path) -> PathBuf {
    let dir = home.join("work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const TERMINAL_RULES: &str = r#"
[[rule]]
user = "run it"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "echo from-the-tool" }

[[rule]]
tool_result = "policy violation"
text = "the hook blocked it"

[[rule]]
tool_result = "from-the-tool"
text = "the tool ran"
"#;

fn run_terminal_turn(home: &Path) -> String {
    let rules = home.join("rules.toml");
    std::fs::write(&rules, TERMINAL_RULES).unwrap();
    let out = command(home)
        .args([
            "-z",
            "run it",
            "--yolo",
            "--mock-script",
            rules.to_str().unwrap(),
        ])
        .current_dir(work(home))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out)
}

fn guarded_home() -> (tempfile::TempDir, PathBuf) {
    let home = mock_home();
    let guard = script(
        home.path(),
        "guard.sh",
        "cat > /dev/null\necho 'policy violation' >&2\nexit 2",
    );
    append_config(
        home.path(),
        &format!(
            "\n[[hooks.pre_tool_call]]\ncommand = \"{}\"\nmatcher = \"terminal\"\n",
            guard.display()
        ),
    );
    (home, guard)
}

#[cfg(unix)]
#[test]
fn exit_2_blocks_a_tool_call_once_the_hook_is_approved() {
    // Spec scenario "Exit 2 blocks a tool call".
    let (home, guard) = guarded_home();
    // Unapproved, a one-shot run (not a foreground chat) skips it.
    assert_eq!(run_terminal_turn(home.path()), "the tool ran\n");
    ok(&run(
        home.path(),
        &[
            "plugins",
            "hooks",
            "allow",
            "pre_tool_call",
            guard.to_str().unwrap(),
        ],
    ));
    assert_eq!(run_terminal_turn(home.path()), "the hook blocked it\n");
    let tool_rows: Vec<String> = session_messages(home.path(), &newest_session(home.path()))
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(tool_rows, ["policy violation"]);
}

#[cfg(unix)]
#[test]
fn an_edited_script_needs_approval_again() {
    // Spec scenario "Edited script needs re-approval".
    let (home, guard) = guarded_home();
    ok(&run(
        home.path(),
        &[
            "plugins",
            "hooks",
            "allow",
            "pre_tool_call",
            guard.to_str().unwrap(),
        ],
    ));
    let listed = ok(&run(home.path(), &["plugins", "hooks", "list"]));
    assert!(
        listed.contains("matcher=terminal  timeout=60s  approved"),
        "{listed}"
    );
    std::fs::write(&guard, "#!/bin/sh\necho 'edited' >&2\nexit 2\n").unwrap();
    let listed = ok(&run(home.path(), &["plugins", "hooks", "list"]));
    assert!(listed.contains("changed since approval"), "{listed}");
    assert_eq!(run_terminal_turn(home.path()), "the tool ran\n");
    let log = std::fs::read_to_string(home.path().join("profiles/default/logs/prometheus.log"))
        .unwrap_or_default();
    assert!(
        log.contains("skipped: changed since approval") && log.contains("default"),
        "{log}"
    );
    ok(&run(
        home.path(),
        &[
            "plugins",
            "hooks",
            "allow",
            "pre_tool_call",
            guard.to_str().unwrap(),
        ],
    ));
    let listed = ok(&run(home.path(), &["plugins", "hooks", "list"]));
    assert!(listed.contains("approved"), "{listed}");
}

#[test]
fn hooks_test_shows_the_fail_closed_outcome() {
    // Spec scenario "Test shows fail-closed outcome".
    let home = mock_home();
    append_config(
        home.path(),
        "\n[[hooks.pre_tool_call]]\ncommand = \"/nonexistent/guard --strict\"\nfail_closed = true\n",
    );
    let out = run(
        home.path(),
        &[
            "plugins",
            "hooks",
            "test",
            "pre_tool_call",
            "--for-tool",
            "terminal",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(shown.contains(r#""action":"block""#), "{shown}");
    assert!(
        shown.contains("hook /nonexistent/guard --strict failed closed"),
        "{shown}"
    );
}

#[test]
fn a_shell_hook_cannot_take_the_classification_hook() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[[hooks.transform_api_error_classification]]\ncommand = \"/bin/true\"\n",
    );
    let out = run(home.path(), &["plugins", "hooks", "list"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("only plugins can"),
        "{}",
        stderr(&out)
    );
}

#[cfg(unix)]
#[test]
fn doctor_reports_a_hook_that_is_not_executable_or_approved() {
    let home = mock_home();
    let hook = script(
        home.path(),
        "ctx.sh",
        "cat > /dev/null\necho '{\"context\":\"hi\"}'",
    );
    append_config(
        home.path(),
        &format!(
            "\n[[hooks.pre_llm_call]]\ncommand = \"{}\"\n",
            hook.display()
        ),
    );
    let out = run(home.path(), &["plugins", "hooks", "doctor"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("pending approval"),
        "{}",
        stdout(&out)
    );
    ok(&run(
        home.path(),
        &[
            "plugins",
            "hooks",
            "allow",
            "pre_llm_call",
            hook.to_str().unwrap(),
        ],
    ));
    let out = run(home.path(), &["plugins", "hooks", "doctor"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains(": ok ("), "{}", stdout(&out));
}

#[cfg(all(unix, feature = "gateway"))]
mod gateway {
    use std::path::Path;
    use std::time::Duration;

    use super::common::gateway::{Daemon, gateway_home, send, wait, wait_said};
    use super::common::{ok, run};

    fn alert_dir(home: &Path) {
        let dir = home.join("profiles/default/hooks/alert");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("HOOK.toml"),
            "name = \"alert\"\ndescription = \"Pages someone\"\nevents = [\"post_gateway_admission\"]\n",
        )
        .unwrap();
        let handler = dir.join("handler");
        std::fs::write(&handler, "#!/bin/sh\ncat > \"$TMPDIR/alert-ran.json\"\n").unwrap();
        std::fs::set_permissions(
            &handler,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
    }

    fn marker(home: &Path) -> std::path::PathBuf {
        home.join("profiles/default/cache/hooks/alert-ran.json")
    }

    #[test]
    fn an_unapproved_hook_directory_is_inert() {
        // Spec scenario "Unapproved hook directory is inert".
        let home = gateway_home("");
        let home = home.path();
        alert_dir(home);
        let daemon = Daemon::start(home);
        send(home, "default", "u1", "hello");
        wait_said(home, "default", "fast done");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker(home).exists());
        let listed = ok(&run(home, &["plugins", "hooks", "list"]));
        assert!(
            listed.contains("alert  post_gateway_admission  pending approval"),
            "{listed}"
        );
        // Approved, its handler runs for the next gateway event.
        ok(&run(home, &["plugins", "hooks", "allow", "--dir", "alert"]));
        send(home, "default", "u1", "again");
        wait("the approved handler", Duration::from_mins(1), || {
            marker(home).exists()
        });
        let seen = std::fs::read_to_string(marker(home)).unwrap();
        assert!(
            seen.contains("\"hook_event_name\":\"post_gateway_admission\""),
            "{seen}"
        );
        daemon.stop(home);
    }
}

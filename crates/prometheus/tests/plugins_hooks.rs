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

/// A foreground interactive chat asks about each unapproved shell hook
/// before it starts; `y` stores the consent, anything else leaves it
/// pending (spec: plugin-system, Shell hooks with first-use consent).
#[cfg(unix)]
#[test]
fn a_foreground_chat_asks_for_consent_before_it_starts() {
    use expectrl::Expect as _;

    use self::common::pty;

    let (home, guard) = guarded_home();
    let question = format!("Shell hook on pre_tool_call: `{}`", guard.display());
    for (answer, status) in [("n", "pending approval"), ("y", "approved")] {
        let mut p = pty::spawn(home.path(), &[]);
        p.expect(question.as_str()).unwrap();
        p.expect("Run it in this profile's sessions? [y/N] ")
            .unwrap();
        p.send_line(answer).unwrap();
        pty::prompt(&mut p);
        pty::exit(p);
        let listed = ok(&run(home.path(), &["plugins", "hooks", "list"]));
        assert!(
            listed.contains(&format!("matcher=terminal  timeout=60s  {status}")),
            "after {answer}: {listed}"
        );
    }
    // Consented, the hook now runs in every session of the profile.
    assert_eq!(run_terminal_turn(home.path()), "the hook blocked it\n");
}

#[cfg(all(unix, feature = "gateway"))]
mod gateway {
    use std::path::Path;
    use std::time::Duration;

    use super::common::gateway::{Daemon, gateway_home, gateway_home_with, send, wait, wait_said};
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

    /// A Hermes hook directory (`HOOK.yaml`) whose handler keeps every
    /// document it receives as `<TMPDIR>/ev-<pid>.json`.
    fn hermes_dir(home: &Path) {
        let dir = home.join("profiles/default/hooks/watcher");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("HOOK.yaml"),
            "name: watcher\ndescription: Watches turns\nevents:\n  - gateway:startup\n  - agent:step\n  - agent:end\n",
        )
        .unwrap();
        let handler = dir.join("handler");
        std::fs::write(
            &handler,
            "#!/bin/sh\nf=\"$TMPDIR/ev-$$\"\ncat > \"$f.part\" && mv \"$f.part\" \"$f.json\"\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &handler,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
    }

    /// The documents the handler received, by hook name.
    fn received(home: &Path) -> Vec<serde_json::Value> {
        let dir = home.join("profiles/default/cache/hooks");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
            .collect()
    }

    #[test]
    fn a_translated_hermes_hook_receives_step_end_and_startup_fields() {
        let rules = "[[rule]]\nuser = \"go\"\n[[rule.tool_calls]]\nname = \"read_file\"\n\
                     arguments = { path = \"page.txt\" }\n\n[[rule]]\ntext = \"";
        let home = gateway_home_with(&format!("{rules}{}\"\n", "y".repeat(600)), "");
        let home = home.path();
        hermes_dir(home);
        ok(&run(
            home,
            &["plugins", "hooks", "allow", "--dir", "watcher"],
        ));
        let daemon = Daemon::start(home);
        send(home, "default", "u1", "go");
        wait_said(home, "default", "yyyy");
        let named = |docs: &[serde_json::Value], hook: &str| {
            docs.iter()
                .filter(|d| d["hook_event_name"] == hook)
                .cloned()
                .collect::<Vec<_>>()
        };
        wait(
            "the agent:end and startup handlers",
            Duration::from_mins(1),
            || {
                let docs = received(home);
                !named(&docs, "on_turn_end").is_empty()
                    && !named(&docs, "on_gateway_startup").is_empty()
            },
        );
        let docs = received(home);
        let startup = named(&docs, "on_gateway_startup");
        assert_eq!(
            startup[0]["extra"]["platforms"],
            serde_json::json!(["test"])
        );
        let steps = named(&docs, "post_llm_call");
        assert!(
            steps.iter().any(|s| s["extra"]["iteration"] == 1
                && s["extra"]["tool_names"] == serde_json::json!(["read_file"])),
            "{steps:?}"
        );
        let end = &named(&docs, "on_turn_end")[0];
        assert_eq!(end["extra"]["response"], "y".repeat(500));
        daemon.stop(home);
    }
}

//! Plugin CLI subcommands and approval transports (spec: plugin-system,
//! Plugin CLI subcommands; Approval transports), with the
//! `testing-hooks` plugin.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{command, mock_home, ok, run, stderr, stdout};

fn place(home: &Path, id: &str, capabilities: &str, settings: &[(&str, &str)]) {
    let dir = home.join("profiles/default/plugins").join(id);
    pmt_plugins::testkit::install("testing-hooks", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"hook@1.3\"]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
    ok(&run(
        home,
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(home, &["plugins", "enable", id, "--grant", "all"]));
    for (key, value) in settings {
        ok(&run(
            home,
            &[
                "config",
                "set",
                &format!("plugins.entries.{id}.{key}"),
                value,
            ],
        ));
    }
}

#[test]
fn a_plugin_command_runs_with_its_exit_code_and_shows_in_help() {
    let home = mock_home();
    place(
        home.path(),
        "tools",
        "cli_commands = [\"status\", \"fail\", \"crash\"]",
        &[("cli", "\"status,fail,crash,secret\"")],
    );
    let out = run(home.path(), &["plugins", "run", "tools", "status", "--now"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "ran status --now as default tty=false cols=80\n"
    );
    let listed = ok(&run(home.path(), &["plugins", "run", "tools"]));
    assert!(
        listed.contains("status  test cli command status"),
        "{listed}"
    );
    // `secret` is not in the grant, so it was never registered.
    assert!(!listed.contains("secret"), "{listed}");
    let out = run(home.path(), &["plugins", "run", "tools", "secret"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    let out = run(home.path(), &["plugins", "run", "tools", "fail"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stderr(&out), "failed on purpose\n");
    let out = run(home.path(), &["plugins", "run", "tools", "crash"]);
    assert_eq!(out.status.code(), Some(70), "{}", stderr(&out));
    let help = run(home.path(), &["help"]);
    assert_eq!(help.status.code(), Some(0));
    let text = stdout(&help);
    assert!(
        text.contains("plugin commands:\n  prometheus plugins run tools status"),
        "{text}"
    );
}

#[test]
fn a_plugin_command_streams_its_output_before_it_exits() {
    // Spec: Plugin CLI subcommands stream stdout and stderr. The command
    // waits for the state key `go`, written only once `started` was read.
    use std::io::{BufRead as _, BufReader, Read as _};

    let home = mock_home();
    place(
        home.path(),
        "tools",
        "cli_commands = [\"stream\"]",
        &[("cli", "\"stream\"")],
    );
    let mut child = command(home.path())
        .args(["plugins", "run", "tools", "stream"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line, "started\n");
    line.clear();
    err.read_line(&mut line).unwrap();
    assert_eq!(line, "working\n");
    assert!(child.try_wait().unwrap().is_none(), "still running");
    let kv = home.path().join("profiles/default/plugin-data/tools/kv");
    std::fs::create_dir_all(&kv).unwrap();
    // Key `go`, hex-encoded.
    std::fs::write(kv.join("676f"), "1").unwrap();
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "finished\n");
    assert!(child.wait().unwrap().success());
}

fn work(home: &Path) -> PathBuf {
    let dir = home.join("work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A one-shot turn writing `AGENTS.md`, a protected file whose write asks
/// once every time.
fn write_protected(home: &Path) -> String {
    let rules = home.join("rules.toml");
    std::fs::write(
        &rules,
        r#"
[[rule]]
user = "write"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "AGENTS.md", content = "be nice" }

[[rule]]
tool_result = "BLOCKED"
text = "denied"

[[rule]]
repeat = true
text = "written"
"#,
    )
    .unwrap();
    let out = command(home)
        .args(["-z", "write", "--mock-script", rules.to_str().unwrap()])
        .current_dir(work(home))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success() || out.status.code() == Some(4),
        "{}",
        stderr(&out)
    );
    stdout(&out)
}

#[test]
fn a_transport_cannot_widen_a_once_only_request() {
    // Spec scenario "Transport cannot widen scope".
    let home = mock_home();
    place(
        home.path(),
        "pager",
        "approval_transport = true",
        &[
            ("transports", "\"phone\""),
            ("transport_mode", "\"always\""),
        ],
    );
    ok(&run(
        home.path(),
        &["config", "set", "plugins.approval_transport", "pager:phone"],
    ));
    assert_eq!(write_protected(home.path()), "denied\n");
    assert!(!work(home.path()).join("AGENTS.md").exists());
    // The same request answered with a choice it offers goes through.
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "plugins.entries.pager.transport_mode",
            "\"once\"",
        ],
    ));
    assert_eq!(write_protected(home.path()), "written\n");
    assert!(work(home.path()).join("AGENTS.md").exists());
}

#[test]
fn a_missing_transport_denies_unless_the_fallback_is_builtin() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "plugins.approval_transport",
            "nobody:phone",
        ],
    ));
    let rules = home.path().join("rules.toml");
    std::fs::write(
        &rules,
        "[[rule]]\nuser = \"clean\"\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"git reset --hard\" }\n\n[[rule]]\ntool_result_regex = 'not registered'\ntext = \"transport missing\"\n\n[[rule]]\nrepeat = true\ntext = \"other\"\n",
    )
    .unwrap();
    let out = command(home.path())
        .args(["-z", "clean", "--mock-script", rules.to_str().unwrap()])
        .current_dir(work(home.path()))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(stdout(&out), "transport missing\n", "{}", stderr(&out));
}

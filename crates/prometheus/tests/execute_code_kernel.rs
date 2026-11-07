//! The persistent `execute_code` kernel and its execution modes in real
//! sessions (spec: subagent-delegation, Persistent `execute_code` session
//! kernel; `execute_code` execution modes). Scripts run on the managed
//! Python runtime (seeded from `pmt_pybridge::testkit`; needs uv on PATH).

mod common;

use std::fmt::Write as _;
use std::path::Path;

use common::{append_config, command, mock_home, stderr};
use serde_json::Value;

/// Runs a one-shot chat whose model makes one `execute_code` call per
/// entry of `calls` (each `(code, reset)`), in order; the tool results
/// the model saw, as JSON bodies.
fn run_calls(home: &Path, calls: &[(&str, bool)]) -> Vec<Value> {
    let mut script = String::new();
    for (code, reset) in calls {
        let code = serde_json::to_string(code).unwrap();
        let _ = write!(
            script,
            "[[rule]]\nuser = \"run it\"\n[[rule.tool_calls]]\nname = \"execute_code\"\n\
             arguments = {{ code = {code}, reset = {reset} }}\n\n"
        );
    }
    script.push_str("[[rule]]\nrepeat = true\ntext = \"done\"\n");
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    let out = command(home)
        .current_dir(home)
        .args(["--json", "-z", "run it", "--mock-script"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| body(m["content"].as_str().unwrap_or_default()))
        .collect()
}

/// A tool result's JSON without its untrusted-content wrapper.
fn body(text: &str) -> Value {
    let inner = text
        .split_once(">\n")
        .filter(|_| text.starts_with("<untrusted-content"))
        .map_or(text, |(_, rest)| rest);
    let inner = inner
        .strip_suffix("\n</untrusted-content>")
        .unwrap_or(inner);
    serde_json::from_str(inner).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn setup(extra: &str) -> tempfile::TempDir {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    append_config(
        home.path(),
        &format!("[tools]\nenabled_toolsets = [\"file\", \"code_execution\"]\n\n{extra}"),
    );
    home
}

/// Scenario: State persists between calls; Reset discards state.
#[test]
fn variables_persist_between_calls_until_a_reset() {
    let home = setup("");
    let results = run_calls(
        home.path(),
        &[
            ("x = 41", false),
            ("print(x + 1)", false),
            ("print(x)", true),
        ],
    );
    assert_eq!(results.len(), 3, "{results:?}");
    assert_eq!(results[1]["stdout"], "42\n", "{:?}", results[1]);
    assert!(
        results[2]["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("NameError"),
        "{:?}",
        results[2]
    );
    assert_ne!(results[2]["exit_code"], 0);
}

/// A timed-out cell kills its kernel and says the state was lost; the
/// next call starts clean.
#[test]
fn a_timeout_loses_the_kernel_state_and_says_so() {
    let home = setup("[code_execution]\ntimeout_secs = 3\n");
    let results = run_calls(
        home.path(),
        &[
            ("x = 1", false),
            ("import time\ntime.sleep(60)", false),
            ("print(x)", false),
        ],
    );
    assert_eq!(results[1]["state_lost"], true, "{:?}", results[1]);
    assert!(
        results[1]["kernel"]
            .as_str()
            .unwrap_or_default()
            .contains("were lost"),
        "{:?}",
        results[1]
    );
    assert!(
        results[2]["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("NameError"),
        "{:?}",
        results[2]
    );
}

/// Strict mode runs in a fresh staging directory, not the workspace.
#[test]
fn strict_mode_runs_in_a_staging_directory() {
    let home = setup("[code_execution]\nmode = \"strict\"\n");
    let results = run_calls(home.path(), &[("import os\nprint(os.getcwd())", false)]);
    let cwd = results[0]["stdout"].as_str().unwrap_or_default().trim();
    let workspace = home.path().canonicalize().unwrap();
    assert!(!cwd.is_empty(), "{:?}", results[0]);
    assert_ne!(Path::new(cwd), workspace, "{:?}", results[0]);
    assert!(cwd.contains("kernel-"), "a staging directory: {cwd}");
    assert!(results[0].get("note").is_none(), "{:?}", results[0]);
}

/// Scenario: Broken venv falls back.
#[test]
fn a_project_venv_without_an_interpreter_falls_back_with_a_note() {
    let home = setup("[code_execution]\nmode = \"project\"\n");
    let venv = home.path().join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::fs::write(venv.join("pyvenv.cfg"), "home = /nonexistent\n").unwrap();
    let results = run_calls(home.path(), &[("print('ran')", false)]);
    assert_eq!(results[0]["stdout"], "ran\n", "{:?}", results[0]);
    let note = results[0]["note"].as_str().unwrap_or_default();
    assert!(
        note.contains(".venv") && note.contains("managed Python runtime"),
        "{note}"
    );
}

/// Project mode runs on the project's virtual environment when it works.
#[cfg(unix)]
#[test]
fn project_mode_uses_a_working_project_venv() {
    let home = setup("");
    let mode = |m: &str| {
        common::ok(&common::run(
            home.path(),
            &["config", "set", "code_execution.mode", m],
        ))
    };
    mode("strict");
    let managed = run_calls(home.path(), &[("import sys\nprint(sys.executable)", false)]);
    let managed = managed[0]["stdout"].as_str().unwrap().trim().to_owned();
    let venv = home.path().join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    std::os::unix::fs::symlink(&managed, venv.join("bin/python")).unwrap();
    let base = Path::new(&managed).parent().unwrap();
    std::fs::write(
        venv.join("pyvenv.cfg"),
        format!("home = {}\n", base.display()),
    )
    .unwrap();
    mode("project");
    let results = run_calls(home.path(), &[("import sys\nprint(sys.prefix)", false)]);
    let prefix = results[0]["stdout"].as_str().unwrap_or_default().trim();
    assert_eq!(
        Path::new(prefix).file_name().and_then(|n| n.to_str()),
        Some(".venv"),
        "{:?}",
        results[0]
    );
    assert!(results[0].get("note").is_none(), "{:?}", results[0]);
}

/// On a remote backend no kernel is kept: scripts run one-shot, and the
/// result says so.
#[test]
fn a_remote_backend_runs_one_shot_scripts_and_says_so() {
    let home = setup("");
    let missing = home.path().join("no-such-runtime");
    for (key, value) in [
        ("terminal.backend", "container"),
        ("terminal.container.runtime", missing.to_str().unwrap()),
    ] {
        common::ok(&common::run(home.path(), &["config", "set", key, value]));
    }
    let results = run_calls(home.path(), &[("x = 1", false), ("print(x)", false)]);
    let note = results[0]["note"].as_str().unwrap_or_default();
    assert!(note.contains("one-shot"), "{:?}", results[0]);
    assert!(
        results[1]["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("NameError"),
        "nothing persisted: {:?}",
        results[1]
    );
}

/// The unsandboxed host never follows a link a cell plants in its own
/// cells directory: a cell's output relinked to another file returns
/// nothing of that file, and a link planted at the next cell's source
/// does not make the host write the model's code through it.
#[cfg(unix)]
#[test]
fn links_planted_in_the_cells_directory_are_not_followed() {
    let home = setup("");
    let secret = home.path().join("secret.txt");
    std::fs::write(&secret, "TOP-SECRET-BYTES").unwrap();
    let rc = home.path().join("rc.sh");
    std::fs::write(&rc, "original\n").unwrap();
    let plant = format!(
        "import os, sys\n\
         cells = sys.argv[1]\n\
         base = os.path.join(cells, '00000001')\n\
         os.remove(base + '.out')\n\
         os.symlink({secret:?}, base + '.out')\n\
         os.symlink({rc:?}, os.path.join(cells, '00000002.py'))\n",
        secret = secret.to_str().unwrap(),
        rc = rc.to_str().unwrap(),
    );
    let results = run_calls(home.path(), &[(&plant, false), ("print('second')", false)]);
    assert!(
        !results[0].to_string().contains("TOP-SECRET-BYTES"),
        "{:?}",
        results[0]
    );
    assert_eq!(results[1]["stdout"], "second\n", "{:?}", results[1]);
    assert_eq!(std::fs::read_to_string(&rc).unwrap(), "original\n");
}

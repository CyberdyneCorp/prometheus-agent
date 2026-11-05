//! The first-party `security-guidance` hook plugin (spec:
//! security-boundaries, Security guidance on written code).

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr};

const WRITE: &str = "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"write_file\"\narguments = { path = \"app.py\", content = \"import pickle\\n\\ndef load(request):\\n    return pickle.loads(request.data)\\n\" }\n\n[[rule]]\nstep = 1\ntext = \"done\"\n";

fn write_app(home: &Path) -> String {
    let script = home.join("write.toml");
    std::fs::write(&script, WRITE).unwrap();
    let out = common::command(home)
        .current_dir(home)
        .args(["chat", "--in-process", "--yolo", "--mock-script"])
        .arg(&script)
        .args(["-z", "write the loader"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(ToString::to_string)
        .collect()
}

/// Scenario: Warn mode.
#[test]
fn warn_mode_writes_the_file_and_reports_the_rule() {
    let home = mock_home();
    let home = home.path();
    ok(&run(home, &["plugins", "enable", "security-guidance"]));
    let tool = write_app(home);
    assert!(home.join("app.py").exists(), "the file was written");
    assert!(tool.contains("py-pickle-loads"), "{tool}");
    assert!(tool.contains("never unpickle data"), "{tool}");
}

#[test]
fn block_mode_refuses_the_write_with_the_guidance() {
    let home = mock_home();
    let home = home.path();
    ok(&run(home, &["plugins", "enable", "security-guidance"]));
    ok(&run(
        home,
        &["config", "set", "plugins.security_guidance.mode", "block"],
    ));
    let tool = write_app(home);
    assert!(!home.join("app.py").exists(), "the write was refused");
    assert!(tool.contains("BLOCKED by security-guidance"), "{tool}");
    assert!(tool.contains("py-pickle-loads"), "{tool}");
}

/// Disabled by default: nothing is matched or added.
#[test]
fn without_the_plugin_writes_are_untouched() {
    let home = mock_home();
    let home = home.path();
    let tool = write_app(home);
    assert!(home.join("app.py").exists());
    assert!(!tool.contains("security-guidance"), "{tool}");
}

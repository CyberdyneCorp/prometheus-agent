//! The desktop's workspace panes end to end (spec: desktop-app, File
//! browser pane; Artifacts gallery and suggestions; Preview rail and
//! in-app browser; Annotate mode in the in-app browser): the page in the
//! desktop browser harness against the real daemon, driving
//! `web/e2e/desktop-panes.spec.ts`.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::io::Write as _;

use common::dashboard::playwright;
use common::{append_config, mock_home, run};

const SCRIPT: &str = r#"
[[rule]]
user = "rewrite it again"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "report.html", content = "<h1>Report v2</h1>" }

[[rule]]
user = "write the report"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "report.html", content = "<h1>Report</h1>" }

[[rule]]
tool_result = "report.html"
user = "again"
text = "Rewritten"

[[rule]]
tool_result = "report.html"
text = "Written"

[[rule]]
repeat = true
text = "ok"
"#;

struct Stop<'a>(&'a std::path::Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// Scenario: Large file preview. Scenario: Dismissed suggestion stays
/// dismissed. Scenario: Hidden page keeps its form state. Scenario: Page
/// calls a native command. Scenario: Password field redaction. Scenario:
/// Pins do not send.
#[test]
fn desktop_files_artifacts_preview_and_annotations() {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), SCRIPT).unwrap();
    append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let workspace = home.path().join("profiles/default/workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut big = std::fs::File::create(workspace.join("big.log")).unwrap();
    let line = format!("{}\n", "log line ".repeat(10));
    for _ in 0..=(40 * 1024 * 1024 / line.len()) {
        big.write_all(line.as_bytes()).unwrap();
    }
    big.set_len(40 * 1024 * 1024).unwrap();
    std::fs::write(workspace.join(".gitignore"), "*.tmp\n").unwrap();
    std::fs::write(workspace.join("ignored.tmp"), "scratch").unwrap();
    let _stop = Stop(home.path());
    let harness = common::desktop::start(home.path());
    playwright(
        home.path(),
        "desktop-panes.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
}

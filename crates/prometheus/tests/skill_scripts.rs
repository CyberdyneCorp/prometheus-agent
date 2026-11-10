//! Skill scripts run with the runtimes their skill declares (spec: skills,
//! Skill scripts run sandboxed with managed runtimes): a skill declaring
//! `metadata.prometheus.node` gets the managed Node first on `PATH` for
//! commands that run its files. (The Python half is
//! `python_bridge::a_skill_script_gets_its_environment_and_no_plugin_environment`.)

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;

use common::{command, ok, run, stderr, stdout, temp_home};

#[test]
fn a_node_skill_script_runs_with_the_managed_node() {
    let home = temp_home();
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    let runtime = common::fake_runtime(
        home.path(),
        "node",
        "22.0.0-aaaaaaaaaaaa",
        &[("bin/node", b"#!/bin/sh\necho managed-node \"$@\"\n")],
    );
    std::fs::set_permissions(
        runtime.join("bin/node"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let skill = home.path().join("profiles/default/skills/web-scraper");
    std::fs::create_dir_all(skill.join("scripts")).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: web-scraper\ndescription: Scrapes pages.\nmetadata:\n  prometheus:\n    node:\n      version: \"22\"\n---\nRun the scraper.\n",
    )
    .unwrap();
    std::fs::write(
        skill.join("scripts/scrape.js"),
        "console.log('system node');\n",
    )
    .unwrap();
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nuser = \"go\"\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = \"node {}/scripts/scrape.js\" }}\n\n[[rule]]\ntool_result = \"managed-node\"\ntext = \"scraped\"\n",
            skill.display()
        ),
    )
    .unwrap();
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", script.to_str().unwrap()])
        .current_dir(home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let results: Vec<String> = common::active_messages(home.path())
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(stdout(&out), "scraped\n", "{} {results:?}", stderr(&out));
}

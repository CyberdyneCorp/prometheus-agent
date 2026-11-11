//! The profile's `container` terminal backend seen from the CLI (spec:
//! execution-environments, Project run-recipe verification; File snapshots
//! and undo): `prometheus verify` runs its phases inside the container, and
//! a chat session's file change there (the workspace is a host bind mount)
//! is snapshotted and undone like a local one.
//!
//! These tests need Podman or Docker (`PMT_TEST_CONTAINER_RUNTIME`, default
//! `auto`). Where none is reachable they skip with a notice, unless
//! `PMT_TEST_CONTAINERS=require` (the Linux CI job), which makes a missing
//! runtime a failure.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use pmt_config::schema::{DegradedFeature, SandboxConfig};
use pmt_sandbox::container::Runtime;
use serde_json::Value;

use self::common::{append_config, command, mock_home};

/// A small image every runtime can pull, pinned by digest (the pmt-tools
/// container tests use the same one).
const IMAGE: &str = "docker.io/library/alpine:3@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a";

/// The runtime setting, or `None` (with a notice) when the tests must skip.
fn runtime_setting() -> Option<String> {
    let setting = std::env::var("PMT_TEST_CONTAINER_RUNTIME").unwrap_or_else(|_| "auto".to_owned());
    let rootful_ok = SandboxConfig {
        allow_degraded: vec![DegradedFeature::All],
        ..SandboxConfig::default()
    };
    match Runtime::detect(&setting).and_then(|runtime| runtime.probe(&rootful_ok).map(|_| ())) {
        Ok(()) => Some(setting),
        Err(err) if std::env::var("PMT_TEST_CONTAINERS").as_deref() == Ok("require") => {
            panic!(
                "PMT_TEST_CONTAINERS=require but no runtime: {}",
                err.message()
            )
        }
        Err(err) => {
            eprintln!("skipping container test: {}", err.message());
            None
        }
    }
}

/// A mock home whose terminal backend is a non-persistent container. A
/// rootful daemon (CI's Docker) is accepted with `allow_degraded = ["all"]`
/// as in the pmt-tools container tests.
fn container_home(setting: &str) -> tempfile::TempDir {
    let home = mock_home();
    append_config(
        home.path(),
        &format!(
            "[terminal]\nbackend = \"container\"\n\n[terminal.container]\nruntime = \"{setting}\"\nimage = \"{IMAGE}\"\npersistent = false\n\n[security.sandbox]\nallow_degraded = [\"all\"]\n"
        ),
    );
    home
}

/// Runs the CLI with the runtime client's own configuration reachable
/// (the tests isolate `HOME`; Docker Desktop's socket is named in the real
/// one's `~/.docker`).
fn run(home: &Path, args: &[&str]) -> Output {
    let mut cmd = command(home);
    if std::env::var_os("DOCKER_CONFIG").is_none()
        && let Some(real) = std::env::var_os("HOME").map(PathBuf::from)
        && real.join(".docker").is_dir()
    {
        cmd.env("DOCKER_CONFIG", real.join(".docker"));
    }
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn project(recipe: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::create_dir_all(dir.path().join(".prometheus")).unwrap();
    std::fs::write(dir.path().join(".prometheus/environment.json"), recipe).unwrap();
    std::fs::write(dir.path().join("marker.txt"), "from the host\n").unwrap();
    dir
}

/// Scenario: Failing tests, on the container backend: the phase runs in
/// the container (the project is its `/workspace`, and only the container
/// has `/etc/alpine-release`) and its failure is reported with its tail.
#[test]
fn verify_runs_its_phases_inside_the_container() {
    let Some(setting) = runtime_setting() else {
        return;
    };
    let home = container_home(&setting);
    let dir = project(
        r#"{"phases": [
            {"name": "build", "command": "test -f /etc/alpine-release && cat /workspace/marker.txt"},
            {"name": "test", "command": "echo assertion boom in $(pwd); exit 3"},
            {"name": "start", "command": "while :; do nc -l -p 8080 < /dev/null; done"}
        ], "port": 8080}"#,
    );
    let out = run(
        home.path(),
        &["--json", "verify", dir.path().to_str().unwrap()],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let phases = report["phases"].as_array().unwrap();
    assert_eq!(phases[0]["passed"], true, "{report:#}");
    assert!(
        phases[0]["output_tail"]
            .as_str()
            .unwrap()
            .contains("from the host"),
        "{report:#}"
    );
    assert_eq!(phases[1]["passed"], false);
    assert_eq!(phases[1]["exit_code"], 3);
    assert!(
        phases[1]["output_tail"]
            .as_str()
            .unwrap()
            .contains("assertion boom in /workspace"),
        "{report:#}"
    );
    assert_eq!(phases[2]["passed"], true, "listening inside: {report:#}");
}

/// Scenario: Undo last turn, in a session on the container backend: the
/// file written in the container's mounted workspace is journaled on the
/// host and `prometheus files undo` removes it again.
#[test]
fn a_container_session_write_is_undone_from_the_host() {
    let Some(setting) = runtime_setting() else {
        return;
    };
    let home = container_home(&setting);
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let work = std::fs::canonicalize(work.path()).unwrap();
    let script = home.path().join("write.toml");
    std::fs::write(
        &script,
        "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"write_file\"\narguments = { path = \"notes.txt\", content = \"written in the container\\n\" }\n\n[[rule]]\nstep = 1\ntext = \"done\"\n",
    )
    .unwrap();
    let out = run(
        home.path(),
        &[
            "--json",
            "--cwd",
            work.to_str().unwrap(),
            "--mock-script",
            script.to_str().unwrap(),
            "--yolo",
            "-z",
            "write notes",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        std::fs::read_to_string(work.join("notes.txt")).unwrap(),
        "written in the container\n"
    );
    let session = report["session_id"].as_str().unwrap();
    let undone = run(
        home.path(),
        &["--json", "files", "undo", "--session", session],
    );
    let text = String::from_utf8_lossy(&undone.stdout);
    assert_eq!(undone.status.code(), Some(0), "{text}");
    let undo: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        undo["removed"][0],
        work.join("notes.txt").to_str().unwrap(),
        "{undo:#}"
    );
    assert!(!work.join("notes.txt").exists());
}

/// A profile skill `gcloud-ops` declaring `creds/gcp.json`, which exists.
fn credential_skill(home: &Path) {
    let profile = home.join("profiles/default");
    std::fs::create_dir_all(profile.join("skills/gcloud-ops")).unwrap();
    std::fs::write(
        profile.join("skills/gcloud-ops/SKILL.md"),
        "---\nname: gcloud-ops\ndescription: Operate Google Cloud.\nmetadata:\n  prometheus:\n    credential_files:\n      - path: creds/gcp.json\n        description: Service account key.\n---\nUse gcloud.\n",
    )
    .unwrap();
    std::fs::create_dir_all(profile.join("creds")).unwrap();
    std::fs::write(profile.join("creds/gcp.json"), "{\"key\": \"s3cret\"}").unwrap();
}

/// A one-shot chat that loads the skill, then runs `command` in the
/// container and reads the host copy with `read_file`; the tool results.
fn load_and_run(home: &Path, command: &str) -> Vec<String> {
    let profile = home.join("profiles/default");
    let host_copy = profile.join("creds/gcp.json");
    std::fs::write(
        profile.join("mock.toml"),
        format!(
            "[[rule]]\nuser = \"go\"\n[[rule.tool_calls]]\nname = \"skill_view\"\narguments = {{ name = \"gcloud-ops\" }}\n\n\
             [[rule]]\ntool_result = \"Use gcloud\"\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = {command:?} }}\n\n\
             [[rule]]\ntool_result_regex = \".\"\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = {{ path = {:?} }}\n\n\
             [[rule]]\ntool_result_regex = \".\"\ntext = \"done\"\n",
            host_copy.display().to_string()
        ),
    )
    .unwrap();
    append_config(home, "\n[providers.mock]\nscript = \"mock.toml\"\n");
    let out = run(home, &["-z", "go"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    common::active_messages(home)
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// Scenario: Read-only mount into Docker (spec: skills, Skill credential
/// file passthrough).
#[test]
fn a_granted_credential_file_is_mounted_read_only() {
    let Some(setting) = runtime_setting() else {
        return;
    };
    let home = container_home(&setting);
    credential_skill(home.path());
    common::ok(&run(
        home.path(),
        &["grants", "add", "skill:gcloud-ops", "credential_files"],
    ));
    let results = load_and_run(
        home.path(),
        "cat /prometheus/creds/gcp.json; echo x > /prometheus/creds/gcp.json || echo read-only",
    );
    assert!(
        results[1].contains("s3cret") && results[1].contains("read-only"),
        "{results:?}"
    );
    assert!(
        !results[2].contains("s3cret"),
        "read_file reached the host copy: {results:?}"
    );
}

/// Scenario: No grant, no file.
#[test]
fn without_the_grant_no_credential_file_is_passed() {
    let Some(setting) = runtime_setting() else {
        return;
    };
    let home = container_home(&setting);
    credential_skill(home.path());
    let results = load_and_run(home.path(), "cat /prometheus/creds/gcp.json || echo absent");
    let notice = "they are passed only with the grant skill:gcloud-ops credential_files";
    assert_eq!(
        results.iter().filter(|r| r.contains(notice)).count(),
        1,
        "{results:?}"
    );
    assert!(
        results[1].contains("absent") && !results[1].contains("s3cret"),
        "{results:?}"
    );
}

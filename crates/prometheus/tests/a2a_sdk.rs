//! The official A2A Python SDK against the A2A server of a running daemon
//! (spec: agent-interop, A2A JSON-RPC methods and task lifecycle, task
//! 74.1). The SDK is pinned, in a virtual environment under
//! `target/a2a-sdk` made by the uv on `PATH` on the python-bridge test
//! interpreter (`scripts/ci/install-uv` makes the same one before CI's
//! tests, so none downloads it).

#![cfg(feature = "gateway")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use self::common::a2a::{self, token_of};
use self::common::{append_config, daemon, mock_home};

/// The SDK release the test drives (kept in step with
/// `scripts/ci/install-uv`).
const VERSION: &str = "1.3.0";
/// Nothing published after this resolves, so the SDK's own dependencies
/// stay what they were when the pin was chosen.
const EXCLUDE_NEWER: &str = "2026-11-01T00:00:00Z";

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
        PathBuf::from,
    )
}

/// The environment's interpreter, made on first use.
fn python() -> PathBuf {
    let dir = target_dir().join("a2a-sdk");
    let python = if cfg!(windows) {
        dir.join("Scripts/python.exe")
    } else {
        dir.join("bin/python")
    };
    let marker = dir.join(format!(".a2a-sdk-{VERSION}"));
    if marker.exists() {
        return python;
    }
    let uv = pmt_pybridge::testkit::uv();
    let made = Command::new(&uv)
        .args([
            "venv",
            "--quiet",
            "--python-preference",
            "only-managed",
            "--python",
        ])
        .arg(pmt_pybridge::testkit::VERSION.as_str())
        .arg(&dir)
        .env(
            "UV_PYTHON_INSTALL_DIR",
            pmt_pybridge::testkit::python_root(),
        )
        .status()
        .unwrap();
    assert!(made.success(), "uv could not make {}", dir.display());
    let installed = Command::new(&uv)
        .args([
            "pip",
            "install",
            "--quiet",
            "--exclude-newer",
            EXCLUDE_NEWER,
            "--python",
        ])
        .arg(&python)
        .arg(format!("a2a-sdk=={VERSION}"))
        .status()
        .unwrap();
    assert!(
        installed.success(),
        "uv could not install a2a-sdk {VERSION}"
    );
    std::fs::write(&marker, "").unwrap();
    python
}

/// Scenario: Interoperates with the official SDK.
#[test]
fn the_official_sdk_resolves_the_card_and_gets_completed_tasks() {
    let home = mock_home();
    let rules = home.path().join("a2a-script.toml");
    std::fs::write(&rules, "[[rule]]\ntext = \"hello, sdk\"\nrepeat = true\n").unwrap();
    append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = {:?}\n",
            rules.to_string_lossy()
        ),
    );
    a2a::listen(home.path());
    a2a::enable(home.path(), &["sdk"], &["sdk"], "");
    let _daemon = daemon::start(home.path(), &[]);
    let port = a2a::port(home.path());
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/a2a/a2a_sdk.py");
    let out = Command::new(python())
        .arg(&script)
        .args([&format!("http://127.0.0.1:{port}"), &token_of("sdk")])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        report["card"]["url"],
        format!("http://127.0.0.1:{port}/"),
        "{report}"
    );
    assert_eq!(report["card"]["streaming"], true);
    assert_eq!(report["send"]["state"], "TASK_STATE_COMPLETED", "{report}");
    assert_eq!(report["send"]["reply"], "hello, sdk");
    assert_eq!(report["get"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(report["get"]["reply"], "hello, sdk");
    assert_eq!(report["get"]["history"], 2);
    assert_eq!(
        report["stream"]["state"], "TASK_STATE_COMPLETED",
        "{report}"
    );
    assert_eq!(report["stream"]["reply"], "hello, sdk");
    assert_eq!(report["stream"]["context_id"], report["send"]["context_id"]);
    assert_eq!(report["list"]["total"], 2);
}

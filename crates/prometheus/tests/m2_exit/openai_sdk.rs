//! The official OpenAI Python SDK, pinned, in a virtual environment under
//! `target/openai-sdk` made by the uv on `PATH` on the python-bridge test
//! interpreter (`scripts/ci/install-uv` makes the same one before CI's
//! tests, so none downloads it).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The SDK release the exit test drives (kept in step with
/// `scripts/ci/install-uv`).
pub(crate) const VERSION: &str = "2.8.0";
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
pub(crate) fn python() -> PathBuf {
    let dir = target_dir().join("openai-sdk");
    let python = if cfg!(windows) {
        dir.join("Scripts/python.exe")
    } else {
        dir.join("bin/python")
    };
    let marker = dir.join(format!(".openai-{VERSION}"));
    if marker.exists() {
        return python;
    }
    let uv = pmt_pybridge::testkit::uv();
    let interpreters = pmt_pybridge::testkit::python_root();
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
        .env("UV_PYTHON_INSTALL_DIR", &interpreters)
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
        .arg(format!("openai=={VERSION}"))
        .status()
        .unwrap();
    assert!(installed.success(), "uv could not install openai {VERSION}");
    std::fs::write(&marker, "").unwrap();
    python
}

/// Runs the SDK script against `base_url` with `key`; its JSON report.
pub(crate) fn drive(base_url: &str, key: &str) -> serde_json::Value {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/m2_exit/openai_sdk.py");
    let out = Command::new(python())
        .arg(&script)
        .args([base_url, key])
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

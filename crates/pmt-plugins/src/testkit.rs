//! Builds the repository's WASM plugins (`plugins/samples/*`,
//! `plugins/testing/*`) into components for tests: `cargo build --release
//! --target wasm32-unknown-unknown` of the plugin workspace, then the core
//! module is turned into a component from the WIT it embeds (what `cargo
//! component` does after building). The build goes to
//! `target/plugin-wasm/`; concurrent test processes are serialized by
//! cargo's own build-directory lock.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// The repository root.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Environment that would change how the outer build compiles (coverage
/// instrumentation, wrappers, a host target) and must not reach the wasm
/// build.
const SCRUBBED: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_TARGET",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_LLVM_COV",
    "LLVM_PROFILE_FILE",
];

static BUILT: Mutex<bool> = Mutex::new(false);

/// The component of plugin crate `name` (`sample-tool`, `testing-spin`),
/// building the plugin workspace on first use in this process.
///
/// # Panics
///
/// When the build or the component encoding fails (a test setup error).
#[must_use]
pub fn component(name: &str) -> Vec<u8> {
    let out = repo_root().join("target/plugin-wasm");
    {
        let mut built = BUILT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*built {
            build(&out);
            *built = true;
        }
    }
    let module = out
        .join("wasm32-unknown-unknown/release")
        .join(format!("{}.wasm", name.replace('-', "_")));
    let bytes = std::fs::read(&module).unwrap_or_else(|e| panic!("{}: {e}", module.display()));
    encode(&bytes)
}

/// Writes the component of `name` into `dir` as `plugin.wasm`.
///
/// # Panics
///
/// As [`component`], and when the file cannot be written.
pub fn install(name: &str, dir: &Path) {
    std::fs::create_dir_all(dir).expect("plugin directory");
    std::fs::write(dir.join(crate::manifest::COMPONENT_FILE), component(name))
        .expect("write plugin.wasm");
}

fn build(out: &Path) {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .current_dir(repo_root().join("plugins"))
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--quiet",
        ])
        .env("CARGO_TARGET_DIR", out)
        .env("CARGO_INCREMENTAL", "0");
    for name in SCRUBBED {
        if *name != "CARGO_TARGET_DIR" {
            command.env_remove(name);
        }
    }
    let status = command
        .status()
        .expect("run cargo for the plugin workspace");
    assert!(status.success(), "building plugins/ for wasm32 failed");
}

/// A core module carrying its WIT (wit-bindgen's custom section) as a
/// component; a component passes through.
fn encode(bytes: &[u8]) -> Vec<u8> {
    if bytes.get(4..8) != Some(&[0x01, 0x00, 0x00, 0x00][..]) {
        return bytes.to_vec();
    }
    wit_component::ComponentEncoder::default()
        .module(bytes)
        .and_then(|e| e.validate(true).encode())
        .unwrap_or_else(|e| panic!("encode component: {e:#}"))
}

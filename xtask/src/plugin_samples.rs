//! `cargo xtask plugin-samples`: builds the sample plugin of every WIT world
//! (`plugins/samples/<world>/`) and the authoring guide's examples
//! (`plugins/examples/<name>/`) with `cargo component` (spec:
//! plugin-system, WIT world versioning and compatibility; tasks 28.1,
//! 28.9).
//!
//! `cargo component` regenerates each sample's `src/bindings.rs` from the
//! WIT before compiling, so the build both proves the WIT and the sample
//! agree and refreshes the checked-in bindings; with `--check` a binding
//! that changed (the WIT moved without regenerating) fails the run. Each
//! output must be a component, not a core module. The host's tests build
//! the same crates with plain cargo from the checked-in bindings.

use std::path::Path;
use std::process::Command;

/// The world of every sample, in the spec's order.
const WORLDS: &[&str] = &[
    "tool",
    "hook",
    "provider",
    "platform-adapter",
    "memory-provider",
    "context-engine",
    "terminal-backend",
    "ui-panel",
    "delegation-router",
];

/// The authoring guide's example plugins (`plugins/examples/<name>/`,
/// crate `example-<name>`).
const EXAMPLES: &[&str] = &["weather"];

/// The component layer of the binary format (`\0asm` + version 0x0d, layer 1).
const COMPONENT_HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];

pub(crate) fn run(root: &Path, check: bool) -> Result<(), String> {
    let samples = WORLDS.iter().map(|w| ("samples", *w, "sample"));
    let examples = EXAMPLES.iter().map(|e| ("examples", *e, "example"));
    for (group, name, prefix) in samples.chain(examples) {
        build(root, check, group, name, prefix)?;
    }
    Ok(())
}

/// Builds `plugins/<group>/<name>` (crate `<prefix>-<name>`).
fn build(root: &Path, check: bool, group: &str, name: &str, prefix: &str) -> Result<(), String> {
    let target = root.join("target/plugin-samples");
    let world = name;
    let manifest = root
        .join("plugins")
        .join(group)
        .join(world)
        .join("Cargo.toml");
    let bindings = root
        .join("plugins")
        .join(group)
        .join(world)
        .join("src/bindings.rs");
    let before = std::fs::read(&bindings).ok();
    let status = Command::new("cargo")
        .current_dir(root.join("plugins"))
        .args([
            "component",
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .arg("--manifest-path")
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .status()
        .map_err(|e| format!("cannot run `cargo component` (install cargo-component): {e}"))?;
    if !status.success() {
        return Err(format!(
            "plugins/{group}/{world} does not build with `cargo component`"
        ));
    }
    let output = target
        .join("wasm32-unknown-unknown/release")
        .join(format!("{prefix}_{}.wasm", world.replace('-', "_")));
    let bytes = std::fs::read(&output).map_err(|e| format!("{}: {e}", output.display()))?;
    if bytes.get(..8) != Some(&COMPONENT_HEADER[..]) {
        return Err(format!("{} is not a component", output.display()));
    }
    if check && std::fs::read(&bindings).ok() != before {
        return Err(format!(
            "plugins/{group}/{world}/src/bindings.rs was stale; commit the regenerated file"
        ));
    }
    println!("{world}: {} bytes", bytes.len());
    Ok(())
}

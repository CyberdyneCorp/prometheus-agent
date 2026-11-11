//! The OCI image (spec: distribution-and-update, Container image run
//! modes; Container filesystem, init and health; Container audio bridge
//! and host services; Network-isolated deployment recipe), checked by
//! `scripts/ci/container-image` against real containers built around this
//! test run's binary: exec user, read-only filesystem, zombie reaping,
//! signal forwarding, in-place restart serving a new profile, the health
//! check, the loopback provider warning and the isolated network.
//!
//! It needs Docker and a Linux binary: on Linux the built binary; elsewhere
//! `PMT_TEST_IMAGE_BINARY` naming a static Linux build. Without them it
//! skips with a notice, unless `PMT_TEST_CONTAINERS=require` (the Linux CI
//! job), which makes that a failure.

mod common;

use std::path::PathBuf;
use std::process::Command;

/// The Linux binary the image is built around, or `None` to skip.
fn linux_binary() -> Option<PathBuf> {
    let required = std::env::var("PMT_TEST_CONTAINERS").as_deref() == Ok("require");
    let docker = Command::new("docker")
        .args(["version", "--format", "{{.Server.Os}}"])
        .output()
        .is_ok_and(|o| o.status.success());
    let binary = std::env::var_os("PMT_TEST_IMAGE_BINARY")
        .map(PathBuf::from)
        .or_else(|| cfg!(target_os = "linux").then(|| PathBuf::from(common::BIN)));
    match (docker, binary) {
        (true, Some(binary)) => Some(binary),
        _ if required => {
            panic!("PMT_TEST_CONTAINERS=require but Docker or a Linux binary is missing")
        }
        _ => {
            eprintln!("skipping the container image test: needs Docker and a Linux binary");
            None
        }
    }
}

#[test]
fn the_image_runs_as_the_spec_says() {
    let Some(binary) = linux_binary() else { return };
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/ci/container-image");
    let out = Command::new("bash")
        .arg(script)
        .arg(&binary)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for check in [
        "exec as the runtime user on the same /data",
        "no zombie build-up after 200 orphaned grandchildren",
        "a stopped daemon runtime is reported unhealthy",
        "isolation verified",
        "a misconfigured network fails the doctor",
    ] {
        assert!(
            stdout.contains(&format!("ok: {check}")),
            "{check}:\n{stdout}"
        );
    }
}

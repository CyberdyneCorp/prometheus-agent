//! How runtimes are launched.

use std::path::PathBuf;

use pmt_tools::computer_use::{PermissionMode, RuntimeSpec};

use super::{backend_argv, check_manifest, serve_args};

fn spec(mode: PermissionMode, manifest: Option<&str>) -> RuntimeSpec {
    RuntimeSpec {
        mode,
        manifest: manifest.map(PathBuf::from),
        session_label: "prometheus-x".into(),
        overlay: false,
    }
}

#[test]
fn private_runtimes_fix_their_mode_and_manifest_at_launch() {
    let unrestricted = serve_args(&spec(PermissionMode::Unrestricted, None), None);
    assert!(
        unrestricted
            .windows(2)
            .any(|w| w == ["--permission-mode", "unrestricted"])
    );
    assert!(
        unrestricted
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals")
    );
    let bounded = serve_args(&spec(PermissionMode::Bounded, Some("/m.yaml")), None);
    assert!(bounded.windows(3).any(|w| w
        == [
            "--capability-manifest",
            "/m.yaml",
            "--approve-capability-manifest"
        ]));
    assert!(
        !bounded
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals")
    );
}

#[test]
fn backends_run_the_driver_of_their_image() {
    assert_eq!(
        backend_argv(&spec(PermissionMode::Standard, None)),
        ["cua-driver", "mcp", "--no-overlay"]
    );
    let private = backend_argv(&spec(PermissionMode::Unrestricted, None));
    assert_eq!(private[..2], ["sh", "-c"]);
    assert!(private.iter().any(|a| a == "unrestricted"));
}

#[test]
fn a_bounded_runtime_needs_a_readable_manifest() {
    assert!(
        check_manifest(None)
            .unwrap_err()
            .contains("capability_manifest")
    );
    let missing =
        check_manifest(Some(std::path::Path::new("/nonexistent/manifest.yaml"))).unwrap_err();
    assert!(missing.contains("/nonexistent/manifest.yaml"), "{missing}");
}

//! `computer-use-backend` plugins as computer-use backends (spec:
//! computer-use, Backend selection and availability).

mod common;

use common::{Fake, Profile};
use pmt_plugins::computer_use::backends;
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_tools::computer_use::{PermissionMode, RuntimeSpec};
use serde_json::{Map, json};

const WORLD: &str = "\"computer-use-backend@1.0\"";

fn spec(mode: PermissionMode, manifest: Option<std::path::PathBuf>) -> RuntimeSpec {
    RuntimeSpec {
        mode,
        manifest,
        session_label: "s1".to_owned(),
        overlay: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sample_backend_registers_launches_and_drives_its_desktop() {
    let mut profile = Profile::new("p");
    profile.install("desktop", "sample-computer-use-backend", WORLD, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let found = backends(&loaded, &CallInfo::default());
    assert_eq!(found.len(), 1);
    let desktop = &found[0];
    assert_eq!(desktop.name, "sample-desktop");
    assert_eq!(desktop.plugin(), "desktop");
    assert_eq!(desktop.check().unwrap(), "in-memory sample desktop");

    let driver = desktop
        .launcher()
        .launch(&spec(PermissionMode::Standard, None))
        .await
        .unwrap();
    assert!(driver.tools().contains(&"get_window_state".to_owned()));
    let state = driver.call("get_window_state", Map::new()).await.unwrap();
    assert!(!state.is_error);
    assert_eq!(state.field("window_title"), Some(&json!("standard:s1")));
    assert_eq!(state.images.len(), 1);
    assert_eq!(state.images[0].0, "image/png");
    let unknown = driver.call("format_disk", Map::new()).await.unwrap();
    assert!(unknown.is_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bounded_runtime_receives_its_manifest_contents() {
    let mut profile = Profile::new("p");
    profile.install("desktop", "sample-computer-use-backend", WORLD, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let desktop = backends(&loaded, &CallInfo::default()).remove(0);
    let launcher = desktop.launcher();
    // The sample refuses a bounded launch without the manifest's contents.
    let missing = profile.path().join("missing.toml");
    let err = launcher
        .launch(&spec(PermissionMode::Bounded, Some(missing.clone())))
        .await
        .err()
        .unwrap();
    assert!(err.contains(&missing.display().to_string()), "{err}");
    let manifest = profile.path().join("manifest.toml");
    std::fs::write(&manifest, "apps = [\"Sample App\"]\n").unwrap();
    let driver = launcher
        .launch(&spec(PermissionMode::Bounded, Some(manifest)))
        .await
        .unwrap();
    let windows = driver.call("list_windows", Map::new()).await.unwrap();
    assert_eq!(
        windows.field("windows").unwrap()[0]["title"],
        json!("bounded:s1")
    );
}

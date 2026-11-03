//! A build without an optional surface (spec: workspace-architecture,
//! Version and build identity). CI runs this file on a build
//! with the dashboard and the adapter kits left out; on a full build it
//! has nothing to check.

#[cfg(any(
    not(feature = "dashboard"),
    all(feature = "gateway", not(feature = "adapter-kits"))
))]
mod common;

#[cfg(not(feature = "dashboard"))]
#[test]
fn a_build_without_the_dashboard_refuses_its_command() {
    let home = common::temp_home();
    let out = common::run(home.path(), &["dashboard", "--port", "9119"]);
    assert_eq!(out.status.code(), Some(69));
    let stderr = common::stderr(&out);
    assert!(
        stderr.contains("surface 'dashboard' is not included in this build"),
        "{stderr}"
    );
}

/// Release builds leave the first-party adapter mocks out (they would push
/// the binary past its size budget); `plugins test-adapter <platform>`
/// says so instead of mistaking the name for an unknown adapter. CI's
/// surface step builds without `adapter-kits`.
#[cfg(all(feature = "gateway", not(feature = "adapter-kits")))]
#[test]
fn a_build_without_the_adapter_kits_refuses_first_party_names() {
    let home = common::temp_home();
    let out = common::run(home.path(), &["plugins", "test-adapter", "telegram"]);
    assert_eq!(out.status.code(), Some(69));
    let stderr = common::stderr(&out);
    assert!(
        stderr.contains("the 'telegram' adapter kit is not included in this build"),
        "{stderr}"
    );
    let out = common::run(home.path(), &["plugins", "test-adapter", "myspace"]);
    assert_eq!(out.status.code(), Some(2), "{}", common::stderr(&out));
}

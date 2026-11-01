//! A build without an optional surface (spec: workspace-architecture,
//! Version and build identity). CI runs this file on a build
//! with the dashboard left out; on a full build it has nothing to check.

#[cfg(not(feature = "dashboard"))]
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

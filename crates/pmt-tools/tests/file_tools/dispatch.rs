//! The file tools through the dispatcher, as the model calls them.

use std::sync::Arc;

use pmt_config::grants::{GrantsFile, SessionGrants};
use pmt_config::schema::{ApprovalsConfig, Config, SecurityConfig};
use pmt_config::{Home, ProfileName};
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::human::Channel;
use pmt_tools::approval::{CommandGate, Context, Floors, GateSetup};
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::protected::ProtectedFiles;
use pmt_tools::result::Status;
use serde_json::json;

use crate::support::{Fixture, body};

#[path = "../common/dispatcher.rs"]
mod common;

#[tokio::test]
async fn an_edit_after_an_out_of_band_change_is_refused_through_the_dispatcher() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        "ses_files",
        SessionEnv {
            files: Some(Arc::new(fx.session())),
            ..SessionEnv::default()
        },
    );
    let dispatcher = common::dispatcher(
        &environments,
        "ses_files",
        &Config::default(),
        &[],
        &fx.outside,
    )
    .await;
    let read = common::call(&dispatcher, "read_file", json!({"path": "a.rs"})).await;
    assert_eq!(read.status, Status::Ok, "{}", read.text_content());
    fx.write("a.rs", "fn changed_elsewhere() {}\n");
    let write = common::call(
        &dispatcher,
        "write_file",
        json!({"path": "a.rs", "content": "fn b() {}\n"}),
    )
    .await;
    assert_eq!(write.status, Status::Error);
    assert_eq!(body(&write)["stale_write_blocked"], true);
    assert_eq!(fx.read("a.rs"), "fn changed_elsewhere() {}\n");

    let search = common::call(
        &dispatcher,
        "search_files",
        json!({"pattern": "fn", "output_mode": "count"}),
    )
    .await;
    assert_eq!(body(&search)["total_matches"], 1);
    let patch = common::call(
        &dispatcher,
        "patch",
        json!({"path": "a.rs", "old_string": "changed_elsewhere", "new_string": "c"}),
    )
    .await;
    assert_eq!(patch.status, Status::Ok, "{}", patch.text_content());
}

#[tokio::test]
async fn a_session_without_an_environment_gets_an_error_result() {
    let fx = Fixture::new();
    let environments = Arc::new(Environments::default());
    let dispatcher = common::dispatcher(
        &environments,
        "ses_none",
        &Config::default(),
        &[],
        &fx.outside,
    )
    .await;
    let result = common::call(&dispatcher, "read_file", json!({"path": "a.rs"})).await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().contains("no execution environment"),
        "{}",
        result.text_content()
    );
}

/// The session's approval gate as a cron run builds it, under yolo: no
/// prompt can be answered and nothing is bypassed but the prompts.
fn cron_gate(fx: &Fixture) -> Arc<CommandGate> {
    let home = Home::open(&fx.outside.join("prometheus-home")).unwrap();
    let profile = home
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let caps = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    Arc::new(CommandGate::new(GateSetup {
        approvals: ApprovalsConfig::default(),
        context: Context::Cron,
        floors: Floors::new(&fx.work).user_home(Some(&fx.home)),
        grants: Arc::new(SessionGrants::new(caps, GrantsFile::of(&profile))),
        protected: ProtectedFiles::from_config(&SecurityConfig::default()).unwrap(),
        channel: Channel::unattended(),
        yolo: true,
    }))
}

/// Spec: device-namespace paths are refused on every platform before any
/// filesystem access, so the approval gate (protected-file and SSH config
/// checks resolve paths) and `search_files` (the capability check resolves
/// its root) must refuse them first.
#[tokio::test]
async fn unc_and_device_paths_are_refused_through_the_dispatcher() {
    let fx = Fixture::new();
    let environments = Arc::new(Environments::default());
    let _attached = environments.attach(
        "ses_unc",
        SessionEnv {
            files: Some(Arc::new(fx.session())),
            ..SessionEnv::default()
        },
    );
    let dispatcher = common::dispatcher_gated(
        &environments,
        "ses_unc",
        &Config::default(),
        &[],
        &fx.outside,
        cron_gate(&fx),
    )
    .await;
    let unc = r"\\attacker.example\share";
    let calls = [
        (
            "write_file",
            json!({"path": format!(r"{unc}\AGENTS.md"), "content": "x"}),
            format!(r"{unc}\AGENTS.md"),
        ),
        (
            "write_file",
            json!({"path": r"\\?\UNC\host\share\.ssh\config", "content": "x"}),
            r"\\?\UNC\host\share\.ssh\config".to_owned(),
        ),
        (
            "patch",
            json!({"path": format!(r"{unc}\a.rs"), "old_string": "a", "new_string": "b"}),
            format!(r"{unc}\a.rs"),
        ),
        (
            "patch",
            json!({"patch": format!(
                "*** Begin Patch\n*** Update File: {unc}\\CLAUDE.md\n@@\n-a\n+b\n*** End Patch"
            )}),
            format!(r"{unc}\CLAUDE.md"),
        ),
        (
            "search_files",
            json!({"pattern": "x", "path": unc}),
            unc.to_owned(),
        ),
        (
            "search_files",
            json!({"pattern": "x", "path": r"\\.\pipe\x"}),
            r"\\.\pipe\x".to_owned(),
        ),
    ];
    for (tool, arguments, path) in calls {
        let result = common::call(&dispatcher, tool, arguments).await;
        assert_eq!(
            result.status,
            Status::Denied,
            "{tool}: {}",
            result.text_content()
        );
        assert_eq!(
            result.text_content(),
            format!("Refusing device-namespace or UNC path '{path}'"),
            "{tool}"
        );
    }
}

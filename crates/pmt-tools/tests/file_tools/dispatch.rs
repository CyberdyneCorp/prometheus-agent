//! The file tools through the dispatcher, as the model calls them.

use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_tools::environments::{Environments, SessionEnv};
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

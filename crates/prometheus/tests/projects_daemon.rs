//! The active project in daemon sessions (spec: projects, Active project
//! and path ownership): a GUI session given no directory works in the
//! active project's primary folder and is recorded under that project;
//! other surfaces keep the profile workspace.

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use pmt_rpc::contracts::ClientKind;
use serde_json::{Value, json};

use self::common::daemon::{self, record};
use self::common::{mock_home, ok, run};

async fn create_as(home: &Path, kind: ClientKind) -> String {
    let mut hello = daemon::hello(false);
    hello.client = kind;
    let (client, _inbox) = pmt_rpc::client::Client::connect(
        &record(home).endpoint,
        pmt_rpc::control::Purpose::Interactive,
        vec!["default".into()],
        &hello,
    )
    .await
    .unwrap();
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

fn session(home: &Path, id: &str) -> Value {
    let shown: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "sessions", "show", id]))).unwrap();
    shown["session"].clone()
}

#[test]
fn a_gui_session_without_a_directory_starts_in_the_active_project() {
    let home = mock_home();
    let api = home.path().join("src/api");
    std::fs::create_dir_all(&api).unwrap();
    let api = std::fs::canonicalize(api).unwrap();
    let api = api.to_str().unwrap();
    ok(&run(
        home.path(),
        &["projects", "create", "api", api, "--use"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let tui = runtime.block_on(create_as(home.path(), ClientKind::Tui));
    let cli = runtime.block_on(create_as(home.path(), ClientKind::Cli));
    assert_eq!(session(home.path(), &tui)["cwd"], api);
    assert_ne!(session(home.path(), &cli)["cwd"], api);
    let listed: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "sessions", "list", "--all", "--project", "api"],
    )))
    .unwrap();
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [tui.as_str()]);
}

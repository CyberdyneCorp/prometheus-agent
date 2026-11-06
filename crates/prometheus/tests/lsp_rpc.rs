//! RPC `lsp.status` (spec: lsp-diagnostics, LSP CLI): the desktop and
//! dashboard see the running language servers `prometheus lsp status`
//! shows.

#![cfg(feature = "gateway")]

mod common;

use pmt_rpc::client::Client;
use pmt_rpc::control::Purpose;
use pmt_tools::lsp::status::{Entry, Status};
use serde_json::json;

use self::common::{daemon, temp_home};

#[test]
fn lsp_status_lists_the_profiles_running_servers() {
    let home = temp_home();
    let home = home.path();
    let _daemon = daemon::start(home, &[]);
    let me = std::process::id();
    Status::of_profile(&home.join("profiles/default")).write(&Entry {
        id: "pyright".into(),
        workspace: "/w/app".into(),
        pid: me,
        started: pmt_core::runtime::process_start_time(me).unwrap(),
        state: "ready".into(),
        trusted_only: false,
        session_id: "ses_x".into(),
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, _inbox) = Client::connect(
            &daemon::record(home).endpoint,
            Purpose::Interactive,
            vec!["default".into()],
            &daemon::hello(false),
        )
        .await
        .unwrap();
        let status = client.call("lsp.status", json!({})).await.unwrap();
        let servers = status["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1, "{status}");
        assert_eq!(servers[0]["id"], "pyright");
        assert_eq!(servers[0]["workspace"], "/w/app");
        assert_eq!(servers[0]["pid"], me);
        assert_eq!(servers[0]["trusted_only"], false);
    });
}

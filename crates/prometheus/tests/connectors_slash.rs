//! `/connectors` on the daemon's client surfaces against the fake Nous
//! connector gateway (spec: connectors, Connectors slash command): the TUI
//! and the desktop run commands through `command.dispatch` with their own
//! client kind, which decides what the listing offers.

mod common;

use std::path::Path;

use common::{daemon, mock_home, ok, run, run_with_stdin};
use pmt_rpc::client::Client;
use pmt_rpc::contracts::ClientKind;
use pmt_rpc::control::Purpose;
use pmt_tools::connectors::fake::Fake;
use serde_json::json;

/// Points the profile at the fake gateway and signs it in to Nous.
fn configure(home: &Path, gateway: &Fake) {
    ok(&run(
        home,
        &["config", "set", "connectors.gateway_url", &gateway.url()],
    ));
    let meta = r#"{"provider":"nous","label":"default","source":"device","obtained_at":"2026-01-01T00:00:00Z","status":"ok"}"#;
    for (name, value) in [
        ("OAUTH_NOUS_DEFAULT_ACCESS", "nous-token"),
        ("OAUTH_NOUS_DEFAULT_META", meta),
    ] {
        ok(&run_with_stdin(
            home,
            &["secrets", "set", name],
            &format!("{value}\n"),
        ));
    }
}

/// A client of `kind` with a fresh session.
async fn session_of(home: &Path, kind: ClientKind) -> (Client, String) {
    let mut hello = daemon::hello(false);
    hello.client = kind;
    let (client, _inbox) = Client::connect(
        &daemon::record(home).endpoint,
        Purpose::Interactive,
        vec!["default".into()],
        &hello,
    )
    .await
    .unwrap();
    let created = client.call("session.create", json!({})).await.unwrap();
    let id = created["session_id"].as_str().unwrap().to_owned();
    (client, id)
}

async fn dispatch(client: &Client, session: &str, args: &str) -> String {
    let reply = client
        .call(
            "command.dispatch",
            json!({"session_id": session, "name": "connectors", "args": args}),
        )
        .await
        .unwrap();
    reply.to_string()
}

#[test]
fn the_tui_lists_accounts_with_states_and_offers_reconnect_for_an_expired_one() {
    let home = mock_home();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let gateway = runtime.block_on(Fake::start(&["gmail"], Vec::new()));
    gateway.add_account("gmail", Some("home"), "me@home.example", "active");
    gateway.add_account("gmail", Some("work"), "me@work.example", "expired");
    configure(home.path(), &gateway);
    let _daemon = daemon::start(home.path(), &[]);
    runtime.block_on(async {
        let (client, session) = session_of(home.path(), ClientKind::Tui).await;
        let listed = dispatch(&client, &session, "").await;
        assert!(
            listed.contains("home  me@home.example  connected"),
            "{listed}"
        );
        assert!(
            listed.contains("work  me@work.example  expired"),
            "{listed}"
        );
        assert!(
            listed.contains("[/connectors reconnect gmail work]"),
            "{listed}"
        );
        assert!(
            !listed.contains("reconnect gmail home"),
            "a connected account needs no reconnect: {listed}"
        );
        assert!(
            listed.contains("[/connectors disconnect gmail work]"),
            "{listed}"
        );

        let asked = dispatch(&client, &session, "disconnect gmail work").await;
        assert!(
            asked.contains("/connectors disconnect gmail work --yes"),
            "{asked}"
        );
        assert_eq!(gateway.accounts().len(), 2, "nothing removed unconfirmed");
        let done = dispatch(&client, &session, "disconnect gmail work --yes").await;
        assert!(done.contains("Disconnected gmail account 'work'"), "{done}");
        assert_eq!(gateway.accounts().len(), 1);
    });
}

#[test]
fn the_line_interface_offers_no_disconnect() {
    let home = mock_home();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let gateway = runtime.block_on(Fake::start(&["gmail"], Vec::new()));
    gateway.add_account("gmail", Some("work"), "me@work.example", "expired");
    configure(home.path(), &gateway);
    let _daemon = daemon::start(home.path(), &[]);
    runtime.block_on(async {
        let (client, session) = session_of(home.path(), ClientKind::Cli).await;
        let listed = dispatch(&client, &session, "list").await;
        assert!(
            listed.contains("[/connectors reconnect gmail work]"),
            "{listed}"
        );
        assert!(!listed.contains("disconnect"), "{listed}");
        let refused = dispatch(&client, &session, "disconnect gmail work --yes").await;
        assert!(
            refused.contains("terminal UI or the desktop app"),
            "{refused}"
        );
        assert_eq!(gateway.accounts().len(), 1, "nothing removed");

        let renamed = dispatch(&client, &session, "rename gmail work --to office").await;
        assert!(renamed.contains("to 'office'"), "{renamed}");
        let reconnect = dispatch(&client, &session, "reconnect gmail office").await;
        assert!(
            reconnect.contains("Open this link to connect gmail"),
            "{reconnect}"
        );
    });
}

//! The vault RPC methods the desktop and dashboard settings pages use
//! (spec: security-boundaries, Vault management CLI): they never return a
//! secret value.

#![cfg(feature = "gateway")]

mod common;

use pmt_rpc::client::Client;
use pmt_rpc::control::Purpose;
use serde_json::json;

use self::common::{daemon, temp_home};

#[test]
fn vault_methods_manage_items_and_never_return_secrets() {
    let home = temp_home();
    let home = home.path();
    let _daemon = daemon::start(home, &[]);
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
        let added = client
            .call(
                "vault.add",
                json!({"label": "GitHub", "login": {"origins": ["https://github.com/login"],
                    "identifier": "octocat", "password": "rpc-secret-991"}}),
            )
            .await
            .unwrap();
        assert_eq!(added["kind"], "login");
        assert_eq!(added["origins"][0], "https://github.com");
        let listed = client.call("vault.list", json!({})).await.unwrap();
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        for reply in [&added, &listed] {
            assert!(!reply.to_string().contains("rpc-secret-991"), "{reply}");
        }
        let both = client
            .call(
                "vault.add",
                json!({"label": "x", "card": {"holder": "A", "number": "4111", "expiry": "1/29", "cvc": "1"},
                    "address": {"name": "A", "line1": "B", "city": "C", "postal_code": "D", "country": "E"}}),
            )
            .await;
        assert!(both.is_err(), "{both:?}");
        let sources = client
            .call("vault.sources", json!({"disable": "onepassword"}))
            .await
            .unwrap();
        assert_eq!(sources["sources"][0]["name"], "onepassword");
        assert_eq!(sources["sources"][0]["enabled"], false);
        let removed = client
            .call("vault.remove", json!({"handle": added["handle"]}))
            .await
            .unwrap();
        assert_eq!(removed["handle"], added["handle"]);
        let listed = client.call("vault.list", json!({})).await.unwrap();
        assert_eq!(listed["items"], json!([]));
    });
}

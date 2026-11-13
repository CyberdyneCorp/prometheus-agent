//! Dashboard plugin contributions over HTTP (spec: web-dashboard,
//! Dashboard plugin contributions; REST endpoint inventory): the
//! contributions in load order with the ignored override, the sandboxed
//! frame document, backend routes served by the plugin host, and rescan.

#![cfg(feature = "dashboard")]

mod common;

use serde_json::{Value, json};

use common::dashboard::{self, call, get, sign_in};
use common::dashboard_plugins;
use common::{daemon, mock_home};

/// Scenario: Override conflict (the API side; the page side is
/// web/e2e/plugins.spec.ts).
#[test]
fn contributions_frames_backends_and_rescan() {
    let home = mock_home();
    dashboard_plugins::install(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let at = |path: &str| get(opened.port, path, &signed.cookie);

    let listed = at("/api/plugins?profile=default");
    assert_eq!(listed.status, 200, "{listed:?}");
    let found = listed.json();
    let ids: Vec<&str> = found["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b", "mute", "weather"], "load order: {found}");
    assert_eq!(found["plugins"][0]["tab"]["override"], "/sessions");
    assert_eq!(found["plugins"][1]["tab"]["override"], Value::Null);
    assert_eq!(
        found["ignored"],
        json!([{"plugin": "b", "route": "/sessions", "winner": "a"}])
    );
    assert_eq!(found["plugins"][3]["backend"], true);
    assert_eq!(found["plugins"][3]["slots"], json!(["header-right"]));

    let frame = at("/api/plugin-frames/a?profile=default&slot=page");
    assert_eq!(frame.status, 200, "{frame:?}");
    let policy = frame.header("content-security-policy").unwrap_or_default();
    assert!(policy.starts_with("sandbox allow-scripts;"), "{policy}");
    assert!(!policy.contains("allow-same-origin"), "{policy}");
    assert!(
        policy.contains("default-src 'none'"),
        "no network: {policy}"
    );
    assert_eq!(frame.header("x-frame-options"), Some("SAMEORIGIN"));
    assert!(frame.body.contains("window.prometheus=Object.freeze"));
    assert!(frame.body.contains("<h1>Plugin A page</h1>"));
    assert_eq!(at("/api/plugin-frames/nope?profile=default").status, 404);

    let forecast = at("/api/plugins/weather/forecast?profile=default&days=2");
    assert_eq!(forecast.status, 200, "{forecast:?}");
    let body = forecast.json();
    assert_eq!(body["forecast"], "sunny");
    assert_eq!(body["request"]["path"], "/forecast");
    assert_eq!(body["request"]["method"], "GET");
    assert_eq!(body["request"]["query"], "profile=default&days=2");
    let refused = call(
        opened.port,
        "POST",
        "/api/plugins/weather/fail?profile=default",
        &signed,
        Some(&json!({})),
    );
    assert_eq!(refused.status, 400, "{refused:?}");
    assert_eq!(refused.json()["message"], "no forecast today");
    assert_eq!(at("/api/plugins/a/anything?profile=default").status, 404);
    assert_eq!(at("/api/plugins?profile=nobody").status, 404);

    // A plugin installed later shows once discovery is rescanned.
    let late = home.path().join("profiles/default/plugins/late");
    std::fs::create_dir_all(late.join("dashboard")).unwrap();
    std::fs::write(late.join("dashboard/index.html"), "<p>late</p>").unwrap();
    std::fs::write(
        late.join("plugin.toml"),
        "manifest_version = 1\nid = \"late\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"late\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\nworlds = []\n\n\
         [dashboard]\nlabel = \"Late\"\nslots = [\"footer-left\"]\n",
    )
    .unwrap();
    dashboard_plugins::enable(home.path(), "late");
    let before = at("/api/plugins?profile=default").json();
    assert_eq!(before["plugins"].as_array().unwrap().len(), 4, "cached");
    let rescanned = call(
        opened.port,
        "POST",
        "/api/plugins/rescan?profile=default",
        &signed,
        Some(&json!({})),
    );
    assert_eq!(rescanned.status, 200, "{rescanned:?}");
    assert_eq!(rescanned.json()["plugins"].as_array().unwrap().len(), 5);
    assert_eq!(
        at("/api/plugins?profile=default").json()["plugins"][2]["id"],
        "late"
    );
}

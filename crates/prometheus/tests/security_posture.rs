//! The security posture check (spec: security-boundaries, Security posture
//! check): `prometheus doctor security [--strict]` reports unsigned
//! plugins, surfaces without allowlists and the egress mode; `--strict`
//! exits 77 when one of them weakens the profile. A plugin package that
//! fails its signature check is recorded in the security audit log.

mod common;

use std::path::{Path, PathBuf};

use common::{ok, run, stderr, stdout, temp_home};

/// A packed sample plugin `weather` (unsigned).
fn package(home: &Path) -> PathBuf {
    let dir = home.join("src/weather");
    pmt_plugins::testkit::install("sample-tool", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"weather\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Forecasts\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"tool@1.0\"]\n",
    )
    .unwrap();
    let out = home.join("weather.pmtpkg");
    ok(&run(
        home,
        &[
            "plugins",
            "pack",
            dir.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    ));
    out
}

/// Scenario: Strict check in CI.
#[test]
fn strict_posture_check_fails_on_an_unsigned_plugin_and_names_it() {
    let home = temp_home();
    let home = home.path();
    // A first run opens the home, which makes its modes owner-only.
    ok(&run(home, &["config", "get", "approvals.mode"]));
    ok(&run(home, &["doctor", "security", "--strict"]));

    let package = package(home);
    let refused = run(home, &["plugins", "install", package.to_str().unwrap()]);
    assert_eq!(refused.status.code(), Some(77), "{}", stderr(&refused));
    let audit =
        std::fs::read_to_string(home.join("profiles/default/logs/security-audit.jsonl")).unwrap();
    assert!(
        audit.contains("\"kind\":\"signature\"") && audit.contains("weather.pmtpkg"),
        "{audit}"
    );
    ok(&run(
        home,
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
        ],
    ));

    let plain = run(home, &["doctor", "security"]);
    assert_eq!(plain.status.code(), Some(0), "{}", stdout(&plain));
    let strict = run(home, &["doctor", "security", "--strict"]);
    assert_eq!(strict.status.code(), Some(77), "{}", stdout(&strict));
    assert!(
        stdout(&strict).contains("unsigned plugin: weather"),
        "{}",
        stdout(&strict)
    );
}

#[test]
fn the_posture_names_open_surfaces_and_the_egress_mode() {
    let home = temp_home();
    let home = home.path();
    ok(&run(
        home,
        &["config", "set", "security.egress.mode", "allowlist"],
    ));
    ok(&run(home, &["config", "get", "security.egress.mode"]));
    common::append_config(
        home,
        "\n[gateway.platforms.telegram]\nenabled = true\nallow_all = true\n",
    );
    let out = run(home, &["doctor", "security", "--strict"]);
    assert_eq!(out.status.code(), Some(77), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("egress: allowlist"), "{text}");
    assert!(
        text.contains("open surface: telegram admits every sender"),
        "{text}"
    );
}

//! The supply-chain advisory catalog against an install (spec:
//! security-boundaries, Supply-chain advisory catalog).

#![cfg(feature = "gateway")]

mod common;

use std::path::Path;

use common::{daemon, ok, run, stderr, temp_home};

/// The advisory of the embedded catalog the fixture's MCP server matches.
const ADVISORY: &str = "PMT-ADV-2024-0001";

fn logs_mentioning(home: &Path, needle: &str) -> usize {
    let mut count = 0;
    for dir in [home.join("logs"), home.join("profiles/default/logs")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                count += text.matches(needle).count();
            }
        }
    }
    count
}

/// A home with an MCP server pinned to a compromised package version.
fn pinned_home() -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(home.path(), &["config", "show"]));
    common::append_config(
        home.path(),
        "\n[mcp.servers.wallet]\nenabled = false\ncommand = \"npx\"\nargs = [\"-y\", \"@solana/web3.js@1.95.6\"]\n",
    );
    home
}

/// Scenario: Compromised MCP package.
#[test]
fn a_pinned_compromised_mcp_package_is_warned_about_until_acknowledged() {
    let home = pinned_home();
    let home = home.path();
    let daemon = daemon::start(home, &[]);
    drop(daemon);
    let warned = logs_mentioning(home, &format!("advisory {ADVISORY}"));
    assert!(
        warned > 0,
        "the gateway logged no warning naming {ADVISORY}"
    );
    let doctor = run(home, &["doctor", "security"]);
    let text = format!("{}{}", common::stdout(&doctor), stderr(&doctor));
    assert!(text.contains(ADVISORY), "{text}");
    assert!(text.contains("@solana/web3.js 1.95.6"), "{text}");
    assert!(text.contains("1.95.8 or later"), "{text}");

    ok(&run(home, &["doctor", "--ack", ADVISORY]));
    let config = std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap();
    assert!(config.contains(ADVISORY), "{config}");
    let warned = logs_mentioning(home, &format!("advisory {ADVISORY}"));
    let daemon = daemon::start(home, &[]);
    drop(daemon);
    assert_eq!(
        logs_mentioning(home, &format!("advisory {ADVISORY}")),
        warned,
        "no new warning after the acknowledgement"
    );
    let doctor = run(home, &["doctor", "security"]);
    let text = format!("{}{}", common::stdout(&doctor), stderr(&doctor));
    assert!(
        text.contains(&format!("{ADVISORY} (acknowledged)")),
        "{text}"
    );
}

#[test]
fn acknowledging_an_unknown_advisory_exits_66() {
    let home = temp_home();
    let out = run(home.path(), &["doctor", "--ack", "PMT-ADV-1999-9999"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown advisory"),
        "{}",
        stderr(&out)
    );
}

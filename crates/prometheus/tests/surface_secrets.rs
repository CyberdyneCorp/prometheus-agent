//! Gateway-owned secrets are never delegable (spec: security-boundaries,
//! Gateway-owned secrets are never delegable): a surface's credential
//! cannot be granted to a plugin, handed to an MCP server or declared by a
//! skill; each refusal says `secret '<NAME>' is surface-only`.

mod common;

use std::path::{Path, PathBuf};

use common::{ok, run, stderr, stdout, temp_home};

/// A packed sample plugin `courier` asking for the Telegram bot token.
fn courier(home: &Path) -> PathBuf {
    let dir = home.join("src/courier");
    pmt_plugins::testkit::install("sample-tool", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"courier\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Posts\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"tool@1.0\"]\n\n[capabilities]\nsecrets = [\"TELEGRAM_BOT_TOKEN\"]\n",
    )
    .unwrap();
    let out = home.join("courier.pmtpkg");
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

#[test]
fn a_plugin_cannot_be_granted_a_bot_token() {
    let home = temp_home();
    let home = home.path();
    let package = courier(home);
    ok(&run(
        home,
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
        ],
    ));
    for args in [
        &["plugins", "enable", "courier", "--grant", "all"][..],
        &["plugins", "grants", "approve", "courier"][..],
    ] {
        let out = run(home, args);
        assert_eq!(out.status.code(), Some(77), "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("secret 'TELEGRAM_BOT_TOKEN' is surface-only"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let grants = ok(&run(home, &["grants", "list"]));
    assert!(!grants.contains("TELEGRAM_BOT_TOKEN"), "{grants}");
}

/// Scenario: Skill requests the bot token (as `skills list` shows it).
#[test]
fn a_skill_declaring_a_bot_token_is_listed_unavailable() {
    let home = temp_home();
    let home = home.path();
    ok(&run(home, &["config", "get", "approvals.mode"]));
    let dir = home.join("profiles/default/skills/bot-helper");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: bot-helper\ndescription: Drives the bot.\nmetadata:\n  prometheus:\n    secrets:\n      - name: TELEGRAM_BOT_TOKEN\n---\nUse the bot.\n",
    )
    .unwrap();
    let listing = ok(&run(home, &["skills", "list"]));
    assert!(
        listing.contains("unavailable: secret 'TELEGRAM_BOT_TOKEN' is surface-only"),
        "{listing}"
    );
}

#[test]
fn an_mcp_server_cannot_receive_a_surface_secret() {
    let home = temp_home();
    let home = home.path();
    ok(&run(home, &["config", "get", "approvals.mode"]));
    common::append_config(
        home,
        "\n[mcp.servers.relay]\ncommand = \"/bin/sh\"\nargs = [\"-c\", \"true\"]\nenv_secrets = [\"DISCORD_BOT_TOKEN\"]\n",
    );
    let out = run(home, &["mcp", "test", "relay"]);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert_ne!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("secret 'DISCORD_BOT_TOKEN' is surface-only"),
        "{text}"
    );
}

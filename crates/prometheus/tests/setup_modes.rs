//! `prometheus setup` modes of M2, scripted on a temporary home (spec: cli,
//! Setup wizard modes): `--quick`, the `tts` and `agent` sections,
//! `--reset`, and `setup gateway <platform>` from the adapter's schema.
//! The wizard's questions are compared with snapshots.

mod common;

use std::path::Path;

use common::{ok, run, run_with_stdin, stderr, temp_home};

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap_or_default()
}

/// The wizard's questions and notes (stderr), compared with a snapshot
/// (`UPDATE_SNAPSHOTS=1` rewrites it).
fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; run with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name}");
}

/// A profile with the mock model configured.
fn configured_home() -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "mock"],
    ));
    home
}

/// Scenario: Quick setup asks only unset values. With a model and no TTS
/// provider, `setup --quick` never asks for the model and asks for the
/// TTS provider (and the other unset values).
#[test]
fn quick_setup_asks_only_for_unset_values() {
    let home = configured_home();
    ok(&run(
        home.path(),
        &["config", "set", "terminal.backend", "local"],
    ));
    // Memory's two switches, then the TTS provider, then the agent's.
    let out = run_with_stdin(home.path(), &["setup", "--quick"], "\n\nedge\n\n\n\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let asked = stderr(&out);
    assert!(!asked.contains("Model — current"), "{asked}");
    assert!(asked.contains("TTS provider:"), "{asked}");
    assert!(config(home.path()).contains("provider = \"edge\""));
    snapshot("setup-quick.txt", &asked);
    // Once set, a second quick run asks for the TTS provider no more.
    let again = run_with_stdin(home.path(), &["setup", "--quick"], "\n\n\n\n\n");
    assert!(
        !stderr(&again).contains("TTS provider:"),
        "{}",
        stderr(&again)
    );
}

/// The agent section: identity file, approval mode, iteration budget.
#[test]
fn the_agent_section_writes_identity_approvals_and_budget() {
    let home = configured_home();
    let out = run_with_stdin(
        home.path(),
        &["setup", "agent"],
        "You are a careful release engineer.\nsmart\n40\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = config(home.path());
    assert!(text.contains("mode = \"smart\""), "{text}");
    assert!(text.contains("max_iterations = 40"), "{text}");
    let soul = std::fs::read_to_string(home.path().join("profiles/default/SOUL.md")).unwrap();
    assert_eq!(soul, "You are a careful release engineer.\n");
}

/// `--reset` restores the section's keys after a confirmation and keeps
/// the secrets; declining changes nothing.
#[test]
fn reset_restores_the_section_defaults_and_keeps_secrets() {
    let home = configured_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "ELEVENLABS_API_KEY"],
        "el-key-0123456789\n",
    ));
    ok(&run(
        home.path(),
        &["config", "set", "tts.provider", "elevenlabs"],
    ));
    let declined = run_with_stdin(home.path(), &["setup", "tts", "--reset"], "n\n");
    assert_eq!(declined.status.code(), Some(1), "{}", stderr(&declined));
    assert!(config(home.path()).contains("elevenlabs"));
    let out = run_with_stdin(home.path(), &["setup", "tts", "--reset"], "y\n");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        !config(home.path()).contains("elevenlabs"),
        "{}",
        config(home.path())
    );
    assert!(
        config(home.path()).contains("provider = \"mock\""),
        "other sections stay"
    );
    let listed = ok(&run(home.path(), &["secrets", "list"]));
    assert!(listed.contains("ELEVENLABS_API_KEY"), "{listed}");
}

/// Scenario: Adapter fields from schema. `setup gateway telegram` asks for
/// the bot token (hidden, stored only in the secrets store) and every
/// scalar field of the Telegram options schema, then enables the adapter.
#[cfg(feature = "gateway")]
#[test]
fn gateway_setup_asks_every_schema_field_and_stores_the_token_as_a_secret() {
    const TOKEN: &str = "123456:telegram-test-token";
    let home = configured_home();
    // The token, then the options by name: ignore_root_dm, mode (a word
    // it does not take, then Enter), poll_timeout_secs (45), and Enter for
    // the rest.
    let out = run_with_stdin(
        home.path(),
        &["setup", "gateway", "telegram"],
        &format!("{TOKEN}\n\nsideways\n\n45\n\n\n\n\n\n"),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let asked = stderr(&out);
    for field in [
        "TELEGRAM_BOT_TOKEN",
        "poll_timeout_secs",
        "mode (polling, webhook)",
        "mode takes (polling, webhook)",
        "webhook_public_url",
        "TELEGRAM_WEBHOOK_SECRET",
        "slash_commands",
        "ignore_root_dm",
        "rich_messages",
    ] {
        assert!(asked.contains(field), "{field} not asked:\n{asked}");
    }
    snapshot("setup-gateway-telegram.txt", &asked);
    let text = config(home.path());
    assert!(
        !text.contains(TOKEN),
        "the token never lands in config.toml"
    );
    assert!(text.contains("[gateway.platforms.telegram]"), "{text}");
    assert!(text.contains("enabled = true"), "{text}");
    assert!(text.contains("poll_timeout_secs = 45"), "{text}");
    let secrets =
        std::fs::read_to_string(home.path().join("profiles/default/secrets.toml")).unwrap();
    assert!(secrets.contains(TOKEN), "stored in the secrets store");
    // An unknown platform is refused.
    let out = run(home.path(), &["setup", "gateway", "carrier-pigeon"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

//! `prometheus setup` modes of M2, scripted on a temporary home (spec: cli,
//! Setup wizard modes): `--quick`, the `tts` and `agent` sections,
//! `--reset`, `--portal` against a fake Nous Portal, the first-run import
//! offer, and `setup gateway <platform>` from the adapter's schema.
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

/// A fake Nous Portal sign-in (device code, then tokens) and the profile
/// config pointing the `nous` sign-in at it. The server lives as long as
/// the returned value.
async fn fake_portal(home: &Path) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_code": "dc-portal", "user_code": "NOUS-SETUP",
            "verification_uri": "https://portal.example/device", "interval": 0, "expires_in": 600
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "portal-access", "refresh_token": "portal-refresh-0123456789",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;
    common::append_config(
        home,
        &format!(
            "[providers.nous.oauth]\ntoken_url = \"{0}/token\"\n\
             device_authorization_url = \"{0}/device\"\nallowed_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    );
    server
}

/// The keys `setup --portal` writes, all at their portal values.
fn assert_portal_config(home: &Path) {
    let text = config(home);
    for line in [
        "provider = \"nous\"",
        "default = \"Hermes-4-405B\"",
        "search_backend = \"nous\"",
        "extract_backend = \"nous\"",
    ] {
        assert!(text.contains(line), "{line} missing:\n{text}");
    }
    for key in ["image_gen.provider", "tts.provider"] {
        let got = ok(&run(home, &["config", "get", key]));
        assert_eq!(got.trim(), "nous", "{key}");
    }
}

/// `setup --portal` signs in, shows the change (`key: old -> new`) and
/// writes it: `nous` with its default model as the main route, and web
/// search, web extract, image and speech through the tool gateway. A
/// second run finds nothing to change.
#[tokio::test(flavor = "multi_thread")]
async fn portal_setup_shows_the_diff_and_routes_model_and_tools_to_nous() {
    let home = configured_home();
    ok(&run(
        home.path(),
        &["config", "set", "tts.provider", "edge"],
    ));
    let _portal = fake_portal(home.path()).await;
    let out = run(home.path(), &["setup", "--portal"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stderr(&out);
    assert!(
        shown.contains("NOUS-SETUP"),
        "the device code is shown:\n{shown}"
    );
    let diff = &shown[shown.find("config.toml changes:").expect(&shown)..];
    for line in [
        "  model.provider: mock -> nous",
        "  model.default: mock -> Hermes-4-405B",
        "  tts.provider: edge -> nous",
    ] {
        assert!(diff.contains(line), "{line} not in the diff:\n{diff}");
    }
    snapshot("setup-portal.txt", diff);
    assert_portal_config(home.path());
    let again = run(home.path(), &["setup", "--portal"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        stderr(&again).contains("nothing to change"),
        "{}",
        stderr(&again)
    );
}

/// Declining the shown change on a terminal writes nothing (exit 1).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn declining_the_portal_change_writes_nothing() {
    use expectrl::{Eof, Expect as _};
    let home = configured_home();
    let _portal = fake_portal(home.path()).await;
    let before = config(home.path());
    let mut p = common::pty::spawn(home.path(), &["setup", "--portal"]);
    p.expect("model.provider: mock -> nous").unwrap();
    p.expect("Write these changes? [y/N]").unwrap();
    p.send_line("n").unwrap();
    p.expect("nothing written").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 1)"), "{status:?}");
    assert_eq!(config(home.path()), before, "config.toml is untouched");
}

/// The first full `setup` of a profile offers to import each agent home
/// it finds; a `y` runs `prometheus migrate` into the profile, and the
/// offer is not made again on the next `setup`.
#[test]
fn the_first_setup_offers_to_import_a_detected_agent_home() {
    let home = temp_home();
    let user = common::user_home(home.path());
    let hermes = common::hermes::Hermes::new(&user.join(".hermes"));
    hermes.write("default", "SOUL.md", "You are calm and precise.\n");
    hermes.write("default", "memories/MEMORY.md", "prefers rust\n");
    let claude = user.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("CLAUDE.md"), "# Style\n\nUse tabs.\n").unwrap();
    // Hermes: yes; Claude Code: no; Enter for every wizard question.
    let answers = format!("y\nn\n{}", "\n".repeat(40));
    let out = run_with_stdin(home.path(), &["setup"], &answers);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let asked = stderr(&out);
    assert!(asked.contains("Import from Hermes ("), "{asked}");
    assert!(asked.contains("Import from Claude Code ("), "{asked}");
    assert!(!asked.contains("did not finish"), "{asked}");
    let soul = std::fs::read_to_string(home.path().join("profiles/default/SOUL.md")).unwrap();
    assert_eq!(
        soul, "You are calm and precise.\n",
        "Hermes' identity imported"
    );
    let memory = ok(&run(home.path(), &["memory", "list"]));
    assert!(memory.contains("prefers rust"), "{memory}");
    assert!(
        !memory.contains("Use tabs"),
        "the declined Claude Code home is not imported: {memory}"
    );
    let again = run_with_stdin(home.path(), &["setup"], &"\n".repeat(40));
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        !stderr(&again).contains("Import from"),
        "asked once per profile: {}",
        stderr(&again)
    );
}

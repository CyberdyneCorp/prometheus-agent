//! The tool configurator's non-interactive forms and the plain-text setup
//! summary (spec: cli, Tool configurator; observability, Plain-text setup
//! summary).

mod common;

use common::{mock_home, ok, run, run_with_stdin, stderr};

fn config(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

/// Scenario: Unknown provider refused.
#[test]
fn an_unknown_provider_is_refused_without_writing() {
    let home = mock_home();
    let before = config(home.path());
    let out = run(home.path(), &["tools", "provider", "web_search", "nosuch"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("auto, firecrawl, searxng"),
        "{}",
        stderr(&out)
    );
    assert_eq!(config(home.path()), before);
    let out = run(home.path(), &["tools", "provider", "tts", "anything"]);
    assert_eq!(out.status.code(), Some(78));
    ok(&run(
        home.path(),
        &["tools", "provider", "web_search", "searxng"],
    ));
    assert!(config(home.path()).contains("search_backend = \"searxng\""));
}

#[test]
fn toolsets_per_surface_and_unknown_names() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["tools", "enable", "web", "--surface", "cron"],
    ));
    let text = config(home.path());
    assert!(text.contains("cron = [\"core\", \"web\"]"), "{text}");
    ok(&run(home.path(), &["tools", "disable", "web"]));
    assert!(config(home.path()).contains("disabled_toolsets = [\"web\"]"));
    let out = run(home.path(), &["tools", "enable", "nosuch"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    let out = run(
        home.path(),
        &["tools", "enable", "web", "--surface", "fridge"],
    );
    assert_eq!(out.status.code(), Some(78));
    let out = run(home.path(), &["tools", "enable", "coding"]);
    assert_eq!(
        out.status.code(),
        Some(78),
        "posture toolsets are never saved"
    );
}

#[test]
fn configure_toggles_a_toolset_and_picks_a_provider() {
    let home = mock_home();
    let out = run_with_stdin(
        home.path(),
        &["tools", "configure", "--surface", "tui"],
        "web\n\nsearxng\nnosuch\n\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = config(home.path());
    assert!(text.contains("tui = [\"core\", \"web\"]"), "{text}");
    assert!(text.contains("search_backend = \"searxng\""), "{text}");
    assert!(
        stderr(&out).contains("unknown web_extract provider 'nosuch'"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_summary_names_the_setup_and_hides_credentials() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "providers.ollama.base_url",
            "https://admin:hunter22@gpu.test:11434/v1",
        ],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENROUTER_API_KEY"],
        "sk-or-v1-abcdefghijklmnopqrstuvwxyz",
    ));
    let out = ok(&run(home.path(), &["debug", "summary"]));
    for section in [
        "version",
        "os",
        "install owner",
        "profile",
        "model",
        "terminal",
        "secrets",
        "toolsets",
        "mcp servers",
        "memory provider",
        "daemon",
        "scheduled jobs",
        "skills",
        "changed config keys:",
    ] {
        assert!(out.contains(section), "{section} missing:\n{out}");
    }
    assert!(out.contains("https://***@gpu.test:11434/v1"), "{out}");
    assert!(!out.contains("hunter22"), "{out}");
    assert!(out.contains("OPENROUTER_API_KEY set"), "{out}");
    assert!(!out.contains("abcdefghijkl"), "{out}");
    let shown = ok(&run(
        home.path(),
        &["debug", "summary", "--show-key-prefixes"],
    ));
    assert!(shown.contains("OPENROUTER_API_KEY sk-o…wxyz"), "{shown}");
}

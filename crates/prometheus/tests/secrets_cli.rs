//! `prometheus secrets` against the built binary (spec: configuration-and-
//! profiles, Secrets store, Secret resolution precedence and scope;
//! security-boundaries, Secrets storage): values enter through stdin or a
//! hidden prompt only and never appear in config, output or messages.

mod common;

use common::{files_under, ok, run, run_with_stdin, stderr, stdout, temp_home};

const TOKEN: &str = "123456:AAH-telegram-bot-token-value";

#[test]
fn value_from_stdin_is_stored_and_never_echoed() {
    let home = temp_home();
    let set = run_with_stdin(
        home.path(),
        &["secrets", "set", "TELEGRAM_BOT_TOKEN"],
        TOKEN,
    );
    ok(&set);
    let list = run(home.path(), &["secrets", "list"]);
    let listed = ok(&list);
    assert!(listed.contains("TELEGRAM_BOT_TOKEN  file"), "{listed}");
    let json = ok(&run(home.path(), &["--json", "secrets", "list"]));
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["schema"], "prometheus.secrets.v1");
    assert_eq!(doc["secrets"][0]["name"], "TELEGRAM_BOT_TOKEN");
    assert_eq!(doc["secrets"][0]["source"], "file");
    let get = ok(&run(
        home.path(),
        &["--json", "secrets", "get", "TELEGRAM_BOT_TOKEN"],
    ));
    for out in [&set, &list] {
        assert!(!stdout(out).contains(TOKEN) && !stderr(out).contains(TOKEN));
    }
    assert!(!json.contains(TOKEN) && !get.contains(TOKEN));
    let profile = home.path().join("profiles/default");
    let secrets = std::fs::read_to_string(profile.join("secrets.toml")).unwrap();
    assert!(secrets.contains(TOKEN), "the store holds the value");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(profile.join("secrets.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Nowhere else in the home: not in config.toml, logs or backups.
    for file in files_under(home.path()) {
        if file.file_name().is_some_and(|n| n == "secrets.toml") {
            continue;
        }
        let bytes = std::fs::read(&file).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(TOKEN),
            "{} holds the secret",
            file.display()
        );
    }
}

#[test]
fn value_on_the_command_line_is_refused() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["secrets", "set", "API_KEY", "sk-live-123456789"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("never accepted as arguments"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("sk-live-123456789"));
    let store = home.path().join("profiles/default/secrets.toml");
    assert!(
        !std::fs::read_to_string(store)
            .unwrap_or_default()
            .contains("sk-live")
    );
}

#[test]
fn reveal_is_refused_without_a_terminal() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-proj-abcdef123456",
    ));
    let out = run(
        home.path(),
        &["secrets", "get", "OPENAI_API_KEY", "--reveal"],
    );
    assert_eq!(out.status.code(), Some(77));
    assert!(out.stdout.is_empty());
    assert!(!stderr(&out).contains("sk-proj"));
}

#[test]
fn missing_secret_names_the_fix() {
    let home = temp_home();
    let out = run(home.path(), &["--json", "secrets", "get", "NOPE"]);
    assert_eq!(out.status.code(), Some(78));
    let err: serde_json::Value = serde_json::from_str(&stderr(&out)).unwrap();
    assert_eq!(err["error"]["code"], "secret.missing");
    assert_eq!(
        err["error"]["hint"],
        "set it with: prometheus secrets set NOPE"
    );
}

#[test]
fn environment_secrets_are_listed_and_shadowed_by_the_store() {
    let home = temp_home();
    let env_value = "env-value-0123456789";
    let list = |home: &std::path::Path| {
        let mut cmd = common::command(home);
        cmd.env("PMT_SECRET_SHARED_KEY", env_value)
            .args(["secrets", "list"]);
        let out = cmd.output().unwrap();
        assert!(!stdout(&out).contains(env_value));
        ok(&out)
    };
    assert!(list(home.path()).contains("SHARED_KEY  env (PMT_SECRET_SHARED_KEY)\n"));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "SHARED_KEY"],
        "store-value-1",
    ));
    let listed = list(home.path());
    assert!(listed.contains("SHARED_KEY  file\n"), "{listed}");
    assert!(
        listed.contains("SHARED_KEY  env (PMT_SECRET_SHARED_KEY)  shadowed"),
        "{listed}"
    );
}

#[test]
fn unset_removes_and_invalid_names_are_usage_errors() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "GONE_KEY"],
        "value-123456",
    ));
    ok(&run(home.path(), &["secrets", "rm", "GONE_KEY"]));
    assert!(!ok(&run(home.path(), &["secrets", "list"])).contains("GONE_KEY"));
    let out = run_with_stdin(home.path(), &["secrets", "set", "lower-case"], "v");
    assert_eq!(out.status.code(), Some(2));
    let out = run_with_stdin(home.path(), &["secrets", "set", "EMPTY_KEY"], "\n");
    assert_eq!(out.status.code(), Some(65));
}

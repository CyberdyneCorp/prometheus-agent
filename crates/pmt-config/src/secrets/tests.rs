//! Secret resolution contracts (spec: configuration-and-profiles, Secret
//! resolution precedence and scope, Secrets are resolved per use).

use std::collections::BTreeMap;
use std::sync::Arc;

use pmt_core::secret::SecretString;

use super::{MemoryKeychain, SecretStore, SecretsHandle, Source};
use crate::schema::SecretsBackend;
use crate::{Home, ProfileName};

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, SecretString> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
        .collect()
}

fn file_store(home: &Home, name: &str) -> SecretStore {
    let profile = home.profile(&ProfileName::parse(name).unwrap()).unwrap();
    SecretStore::open(&profile, SecretsBackend::File)
}

#[test]
fn launch_environment_does_not_leak_to_another_profile() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let launch_env = env(&[("OPENAI_API_KEY", "sk-env-123456")]);
    let default = SecretsHandle::new("default", file_store(&home, "default"), Some(launch_env));
    let other = SecretsHandle::new("home", file_store(&home, "home"), None);
    assert_eq!(
        default.resolve("OPENAI_API_KEY").unwrap().expose(),
        "sk-env-123456"
    );
    let err = other.resolve("OPENAI_API_KEY").unwrap_err();
    assert_eq!(err.code().code, "secret.missing");
    assert_eq!(err.exit_code(), 78);
    assert!(err.message().contains("'OPENAI_API_KEY'") && err.message().contains("'home'"));
    assert_eq!(
        err.hint(),
        Some("set it with: prometheus -p home secrets set OPENAI_API_KEY")
    );
}

#[test]
fn store_is_resolved_before_the_environment() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let store = file_store(&home, "default");
    store
        .set("TOKEN", &SecretString::new("from-store"))
        .unwrap();
    let handle = SecretsHandle::new(
        "default",
        store,
        Some(env(&[("PMT_SECRET_TOKEN", "from-env"), ("OTHER", "x")])),
    );
    assert_eq!(handle.resolve("TOKEN").unwrap().expose(), "from-store");
    let listed = handle.list().unwrap();
    let token: Vec<_> = listed.iter().filter(|r| r.name == "TOKEN").collect();
    assert_eq!(token.len(), 2);
    assert_eq!((token[0].source, token[0].shadowed), (Source::File, false));
    assert_eq!((token[1].source, token[1].shadowed), (Source::Env, true));
    assert_eq!(token[1].variable.as_deref(), Some("PMT_SECRET_TOKEN"));
}

#[test]
fn generic_variable_shadows_the_provider_native_one() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let handle = SecretsHandle::new(
        "default",
        file_store(&home, "default"),
        Some(env(&[("PMT_SECRET_KEY", "generic"), ("KEY", "native")])),
    );
    assert_eq!(handle.resolve("KEY").unwrap().expose(), "generic");
    let shadowed: Vec<_> = handle
        .list()
        .unwrap()
        .into_iter()
        .map(|r| (r.variable.unwrap(), r.shadowed))
        .collect();
    assert_eq!(
        shadowed,
        [
            ("PMT_SECRET_KEY".to_owned(), false),
            ("KEY".to_owned(), true)
        ]
    );
}

#[test]
fn file_store_rotation_is_seen_by_the_next_use() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let handle = SecretsHandle::new("default", file_store(&home, "default"), None);
    // Another process (`prometheus secrets set`) writes the same file.
    let writer = file_store(&home, "default");
    writer
        .set("API_KEY", &SecretString::new("old-value"))
        .unwrap();
    assert_eq!(handle.resolve("API_KEY").unwrap().expose(), "old-value");
    writer
        .set("API_KEY", &SecretString::new("new-value"))
        .unwrap();
    assert_eq!(handle.resolve("API_KEY").unwrap().expose(), "new-value");
}

#[test]
fn keychain_values_are_reread_after_reload() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let keychain = Arc::new(MemoryKeychain::default());
    let store = || SecretStore::with_keychain(&profile, SecretsBackend::Keychain, keychain.clone());
    store()
        .set("API_KEY", &SecretString::new("old-value"))
        .unwrap();
    store()
        .set("UNUSED", &SecretString::new("x-value"))
        .unwrap();
    let handle = SecretsHandle::new("default", store(), None);
    assert_eq!(handle.resolve("API_KEY").unwrap().expose(), "old-value");
    store()
        .set("API_KEY", &SecretString::new("new-value"))
        .unwrap();
    store()
        .set("UNUSED", &SecretString::new("y-value"))
        .unwrap();
    // Within the staleness window the cached value may still be used...
    assert_eq!(handle.resolve("API_KEY").unwrap().expose(), "old-value");
    // ...until /reload-secrets, which names only the used secrets that changed.
    assert_eq!(handle.reload().unwrap(), ["API_KEY"]);
    assert_eq!(handle.resolve("API_KEY").unwrap().expose(), "new-value");
    assert!(handle.reload().unwrap().is_empty());
}

#[test]
fn clones_share_the_cache() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let handle = SecretsHandle::new("default", file_store(&home, "default"), None);
    let clone = handle.clone();
    handle
        .store()
        .set("A_KEY", &SecretString::new("value-1"))
        .unwrap();
    clone.resolve("A_KEY").unwrap();
    handle
        .store()
        .set("A_KEY", &SecretString::new("value-2"))
        .unwrap();
    assert_eq!(handle.reload().unwrap(), ["A_KEY"]);
}

#[test]
fn invalid_names_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let store = file_store(&home, "default");
    for bad in ["lower", "1ABC", "A-B", ""] {
        let err = store.set(bad, &SecretString::new("v")).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{bad}");
    }
}

#[test]
fn debug_never_shows_values() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let handle = SecretsHandle::new(
        "default",
        file_store(&home, "default"),
        Some(env(&[("PMT_SECRET_X", "sk-hidden-value")])),
    );
    assert!(!format!("{handle:?}").contains("sk-hidden"));
}

#[test]
fn migrate_moves_values_and_switches_the_backend() {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let file = SecretStore::open(&profile, SecretsBackend::File);
    file.set("A_KEY", &SecretString::new("value-a")).unwrap();
    file.set("B_KEY", &SecretString::new("value-b")).unwrap();
    let keychain = Arc::new(MemoryKeychain::default());
    let target = SecretStore::with_keychain(&profile, SecretsBackend::Keychain, keychain.clone());
    assert_eq!(super::migrate(&profile, &file, &target).unwrap(), 2);
    assert_eq!(target.get("B_KEY").unwrap().unwrap().expose(), "value-b");
    assert!(file.names().unwrap().is_empty());
    let config = std::fs::read_to_string(profile.config_file().path()).unwrap();
    assert!(config.contains("backend = \"keychain\""), "{config}");
    // And back.
    let back = SecretStore::open(&profile, SecretsBackend::File);
    assert_eq!(super::migrate(&profile, &target, &back).unwrap(), 2);
    assert!(keychain.accounts().is_empty());
    assert_eq!(back.get("A_KEY").unwrap().unwrap().expose(), "value-a");
}

/// Command secret source precedence: the store and the environment win
/// unless `override_existing`; the helper fills names nobody else has.
#[cfg(unix)]
#[test]
fn command_values_rank_after_the_store_unless_they_override() {
    use crate::schema::SecretCommandConfig;

    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(temp.path()).unwrap();
    let store = file_store(&home, "default");
    store
        .set("TOKEN", &SecretString::new("from-store"))
        .unwrap();
    let helper = |override_existing| SecretCommandConfig {
        enabled: true,
        argv: vec![
            "sh".into(),
            "-c".into(),
            "echo TOKEN=from-helper; echo ONLY_HELPER=h1; echo NATIVE=from-helper".into(),
        ],
        timeout_secs: 3,
        override_existing,
    };
    let launch = || Some(env(&[("NATIVE", "from-env")]));
    let ranked = SecretsHandle::new("default", store.clone(), launch())
        .with_command(super::command::load(&helper(false)));
    assert_eq!(ranked.resolve("TOKEN").unwrap().expose(), "from-store");
    assert_eq!(ranked.resolve("NATIVE").unwrap().expose(), "from-env");
    assert_eq!(ranked.resolve("ONLY_HELPER").unwrap().expose(), "h1");
    let listed = ranked.list().unwrap();
    assert!(
        listed
            .iter()
            .any(|r| r.name == "TOKEN" && r.source == Source::Command && r.shadowed)
    );
    let redacted = ranked.redactor().unwrap().redact("value h1 here");
    assert_eq!(redacted, "value [REDACTED:ONLY_HELPER] here");
    let first = SecretsHandle::new("default", store, launch())
        .with_command(super::command::load(&helper(true)));
    assert_eq!(first.resolve("TOKEN").unwrap().expose(), "from-helper");
    assert_eq!(first.resolve("NATIVE").unwrap().expose(), "from-helper");
}

/// Scenario: Helper times out. The helper's whole process group is killed
/// at the timeout, nothing it printed is kept, and loading continues.
#[cfg(unix)]
#[test]
fn a_helper_waiting_for_a_pin_is_killed_at_the_timeout() {
    use super::command::{Failure, run};
    use crate::schema::SecretCommandConfig;

    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("grandchild-survived");
    let script = format!(
        "(sleep 2; touch '{}') & printf 'Enter PIN: '; sleep 30; echo TOKEN=late",
        marker.display()
    );
    let config = SecretCommandConfig {
        enabled: true,
        argv: vec!["sh".into(), "-c".into(), script],
        timeout_secs: 1,
        override_existing: false,
    };
    let started = std::time::Instant::now();
    let failure = run(&config).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(
        failure,
        Failure::Failed("timed out after 1 s; killed".into())
    );
    assert!(failure.line().starts_with("helper command failed"));
    assert!(!failure.line().contains("PIN"));
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(
        !marker.exists(),
        "the helper's process group was not killed"
    );
}

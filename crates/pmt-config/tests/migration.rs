//! Schema versioning end to end (spec: configuration-and-profiles, Config
//! schema versioning and migration): an old file is migrated at load time
//! with a backup, and a file from a newer binary is refused.

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope, SCHEMA_VERSION};

const V0: &str = include_str!("fixtures/config-v0.toml");

fn home() -> (tempfile::TempDir, Home) {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(&temp.path().join("home")).unwrap();
    (temp, home)
}

#[test]
fn version_zero_file_is_migrated_with_a_backup() {
    let (_temp, home) = home();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let config_path = profile.path().join("config.toml");
    std::fs::write(&config_path, V0).unwrap();

    let scope = ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other).unwrap();

    assert_eq!(scope.config().agent.max_iterations, 120);
    assert_eq!(scope.config().approvals.timeout_secs, 300);
    let migrated = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        migrated.contains(&format!("schema_version = {SCHEMA_VERSION}")),
        "{migrated}"
    );
    assert!(
        migrated.contains("# Long refactors need more steps.\nmax_iterations = 120"),
        "{migrated}"
    );
    assert!(!migrated.contains("max_turns ="), "{migrated}");
    // The renamed timeout equals the new default, so it is not persisted.
    assert!(!migrated.contains("timeout ="), "{migrated}");
    assert!(migrated.contains("theme = \"mono\""), "{migrated}");

    let backups: Vec<_> = std::fs::read_dir(profile.path().join("backups/config"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1);
    let name = backups[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with("config-v0-") && name.to_ascii_lowercase().ends_with(".toml"),
        "{name}"
    );
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), V0);

    // A second load finds a current file and takes no new backup.
    ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other).unwrap();
    assert_eq!(
        std::fs::read_dir(profile.path().join("backups/config"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn newer_file_is_refused_with_exit_78() {
    let (_temp, home) = home();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let newer = SCHEMA_VERSION + 1;
    let text = format!("schema_version = {newer}\n");
    std::fs::write(profile.path().join("config.toml"), &text).unwrap();

    let err =
        ProfileScope::open(profile.clone(), &Overrides::default(), Launch::Other).unwrap_err();

    assert_eq!(err.exit_code(), 78);
    assert_eq!(
        err.message(),
        format!(
            "config.toml schema_version {newer} is newer than this prometheus supports ({SCHEMA_VERSION}); upgrade prometheus"
        )
    );
    assert_eq!(
        std::fs::read_to_string(profile.path().join("config.toml")).unwrap(),
        text
    );
}

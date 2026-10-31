//! Profile lifecycle contracts (spec: configuration-and-profiles, Profile
//! name rules, Profile lifecycle commands, Safe profile deletion).

use pmt_core::runtime::{Role, WriterLock};
use pmt_core::secret::SecretString;

use super::CloneFrom;
use crate::{Home, ProfileName};

fn home() -> (tempfile::TempDir, Home) {
    let temp = tempfile::tempdir().unwrap();
    let home = Home::open(&temp.path().join("h")).unwrap();
    (temp, home)
}

fn name(n: &str) -> ProfileName {
    ProfileName::parse(n).unwrap()
}

#[test]
fn invalid_and_reserved_names_create_nothing() {
    let (_t, home) = home();
    let err = home.create_profile("../evil", None).unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.message().contains("invalid profile name"));
    let err = home.create_profile("Tools", None).unwrap_err();
    assert_eq!(err.message(), "profile name 'tools' is reserved");
    assert_eq!(home.profiles().unwrap(), [name("default")]);
}

#[test]
fn existing_profile_is_refused() {
    let (_t, home) = home();
    home.create_profile("work", None).unwrap();
    for existing in ["work", "default"] {
        let err = home.create_profile(existing, None).unwrap_err();
        assert_eq!(
            err.message(),
            format!("profile '{existing}' already exists")
        );
    }
}

#[test]
fn clone_copies_config_once_and_no_secrets() {
    let (_t, home) = home();
    let default = home.profile(&name("default")).unwrap();
    default
        .config_file()
        .set("agent.max_iterations", "7")
        .unwrap();
    std::fs::write(default.path().join("SOUL.md"), "I am default").unwrap();
    std::fs::write(default.path().join("skills/s.md"), "skill").unwrap();
    let store = crate::SecretStore::open(&default, crate::schema::SecretsBackend::File);
    store
        .set("TOKEN", &SecretString::new("secret-value"))
        .unwrap();
    let source = name("default");
    let clone = CloneFrom {
        source: &source,
        secrets: false,
    };
    let coder = home.create_profile("coder", Some(clone)).unwrap();
    let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap();
    assert_eq!(
        read(&coder.path().join("config.toml")),
        read(&default.path().join("config.toml"))
    );
    assert_eq!(read(&coder.path().join("SOUL.md")), "I am default");
    assert_eq!(read(&coder.path().join("skills/s.md")), "skill");
    assert!(!read(&coder.path().join("secrets.toml")).contains("secret-value"));
    // A one-time copy: later changes to the source do not reach the clone.
    default
        .config_file()
        .set("agent.max_iterations", "9")
        .unwrap();
    assert!(read(&coder.path().join("config.toml")).contains("max_iterations = 7"));

    let with_secrets = CloneFrom {
        source: &source,
        secrets: true,
    };
    let copy = home.create_profile("copy", Some(with_secrets)).unwrap();
    assert!(read(&copy.path().join("secrets.toml")).contains("secret-value"));
}

#[test]
fn use_writes_active_profile_and_default_removes_it() {
    let (_t, home) = home();
    let active = home.root().join("active_profile");
    assert!(home.use_profile(&name("nope")).is_err());
    home.create_profile("work", None).unwrap();
    home.use_profile(&name("work")).unwrap();
    assert_eq!(std::fs::read_to_string(&active).unwrap(), "work\n");
    assert_eq!(home.select_profile(None).unwrap(), name("work"));
    assert_eq!(
        home.select_profile(Some("default")).unwrap(),
        name("default")
    );
    home.use_profile(&name("default")).unwrap();
    assert!(!active.exists());
}

#[test]
fn rename_moves_the_directory_and_the_selection() {
    let (_t, home) = home();
    home.create_profile("work", None).unwrap();
    home.use_profile(&name("work")).unwrap();
    let renamed = home.rename_profile(&name("work"), "job").unwrap();
    assert!(renamed.path().join("config.toml").is_file());
    assert!(!home.profile_path(&name("work")).exists());
    assert_eq!(home.active_profile().unwrap(), name("job"));
    assert_eq!(
        home.rename_profile(&name("default"), "x")
            .unwrap_err()
            .exit_code(),
        2
    );
}

#[test]
fn delete_tombstones_and_clears_the_selection() {
    let (_t, home) = home();
    let old = home.create_profile("old", None).unwrap();
    home.use_profile(&name("old")).unwrap();
    home.delete_profile(&name("old")).unwrap();
    assert!(!old.path().exists());
    assert!(home.tombstone(&name("old")).is_file());
    assert_eq!(home.active_profile().unwrap(), name("default"));
    // A holder of the old handle cannot write there again.
    assert_eq!(
        old.ensure_live().unwrap_err().code().code,
        "profile.deleted"
    );
    assert_eq!(
        home.profile(&name("old")).unwrap_err().code().code,
        "profile.deleted"
    );
    assert!(!old.path().exists());
    assert_eq!(home.profiles().unwrap(), [name("default")]);
    // Creating it again is the explicit way back.
    home.create_profile("old", None).unwrap();
    assert!(!home.tombstone(&name("old")).exists());
}

#[test]
fn default_and_in_use_profiles_are_not_deleted() {
    let (_t, home) = home();
    let err = home.delete_profile(&name("default")).unwrap_err();
    assert_eq!(err.message(), "the default profile cannot be deleted");
    home.create_profile("busy", None).unwrap();
    let lock = WriterLock::acquire(home.root(), Role::Chat, vec!["busy".into()]).unwrap();
    let err = home.delete_profile(&name("busy")).unwrap_err();
    assert_eq!(err.exit_code(), 75);
    assert!(err.message().contains("in use"), "{}", err.message());
    assert!(home.profile_path(&name("busy")).is_dir());
    drop(lock);
    home.delete_profile(&name("busy")).unwrap();
}

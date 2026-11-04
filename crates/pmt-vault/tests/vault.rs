//! Credential vault storage contracts (spec: security-boundaries,
//! Credential vault storage).

use std::sync::Arc;

use pmt_config::schema::VaultKeyStorage;
use pmt_config::secrets::{Keychain, MemoryKeychain};
use pmt_core::error::{Error, codes};
use pmt_vault::key::{self, KEY_FILE, KEYREF_FILE, Location};
use pmt_vault::{Item, Kind, Login, Secret, Vault};

fn login(label: &str, password: &str) -> Item {
    Item {
        handle: Kind::Login.new_handle(),
        label: label.to_owned(),
        secret: Secret::Login(Login {
            origins: vec!["https://github.com".to_owned()],
            identifier: "octocat".to_owned(),
            password: password.to_owned(),
            totp: None,
        }),
    }
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// A keychain that is never reachable.
#[derive(Debug)]
struct Unreachable;

impl Keychain for Unreachable {
    fn get(&self, _: &str) -> Result<Option<String>, Error> {
        Err(Error::new(
            &codes::SECRET_KEYCHAIN_UNAVAILABLE,
            "no keychain",
        ))
    }
    fn set(&self, _: &str, _: &str) -> Result<(), Error> {
        Err(Error::new(
            &codes::SECRET_KEYCHAIN_UNAVAILABLE,
            "no keychain",
        ))
    }
    fn delete(&self, _: &str) -> Result<bool, Error> {
        Err(Error::new(
            &codes::SECRET_KEYCHAIN_UNAVAILABLE,
            "no keychain",
        ))
    }
}

#[test]
fn items_are_sealed_and_only_metadata_is_listed() {
    let profile = temp();
    let vault = Vault::of(profile.path(), Arc::new(MemoryKeychain::default()));
    let meta = vault
        .add(login("GitHub", "hunter2-sealed"), VaultKeyStorage::File)
        .unwrap();
    assert_eq!(meta.kind, Kind::Login);
    let sealed = std::fs::read(vault.dir().join("vault.enc")).unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("hunter2-sealed"));
    assert!(!String::from_utf8_lossy(&sealed).contains("octocat"));
    let listed = serde_json::to_string(&vault.list().unwrap()).unwrap();
    assert!(listed.contains("octocat") && listed.contains("https://github.com"));
    assert!(!listed.contains("hunter2-sealed"), "{listed}");
    assert_eq!(
        vault.get(&meta.handle).unwrap().secret_values(),
        ["hunter2-sealed"]
    );
    #[cfg(unix)]
    for file in ["vault.enc", KEY_FILE] {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(vault.dir().join(file))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{file}");
    }
    vault.remove(&meta.handle).unwrap();
    assert!(vault.list().unwrap().is_empty());
}

#[test]
fn a_vault_whose_key_is_missing_fails_and_is_never_rekeyed() {
    let profile = temp();
    let vault = Vault::of(profile.path(), Arc::new(MemoryKeychain::default()));
    vault
        .add(login("GitHub", "pw"), VaultKeyStorage::File)
        .unwrap();
    let before = std::fs::read(vault.dir().join("vault.enc")).unwrap();
    std::fs::remove_file(vault.dir().join(KEY_FILE)).unwrap();
    let err = vault.list().unwrap_err();
    assert_eq!(
        err.message(),
        "vault key not found; restore it or delete vault.enc to start over"
    );
    let err = vault
        .add(login("Other", "pw2"), VaultKeyStorage::File)
        .unwrap_err();
    assert_eq!(err.code().code, "vault.key_not_found");
    assert_eq!(
        std::fs::read(vault.dir().join("vault.enc")).unwrap(),
        before
    );
    assert!(!vault.dir().join(KEY_FILE).exists());
}

/// Scenario: Interrupted key migration.
#[test]
fn an_interrupted_migration_opens_with_the_file_key_and_completes_on_rerun() {
    let profile = temp();
    let keychain = Arc::new(MemoryKeychain::default());
    let vault = Vault::of(profile.path(), Arc::clone(&keychain) as _);
    vault
        .add(login("GitHub", "pw-migrate"), VaultKeyStorage::File)
        .unwrap();
    // The migration wrote the keychain entry, then stopped.
    let file = Location::File(vault.dir().join(KEY_FILE));
    let the_key = key::read(&file, keychain.as_ref()).unwrap().unwrap();
    let entry = Location::Keychain(key::account(vault.dir()));
    key::write(&entry, &the_key, keychain.as_ref()).unwrap();
    assert_eq!(vault.key_location().unwrap(), Some(file.clone()));
    assert_eq!(vault.list().unwrap().len(), 1, "opens with the file key");
    // Running it again completes it.
    assert_eq!(vault.migrate_key(VaultKeyStorage::Keychain).unwrap(), entry);
    assert!(vault.dir().join(KEYREF_FILE).exists());
    assert!(!vault.dir().join(KEY_FILE).exists());
    assert_eq!(vault.list().unwrap().len(), 1);
    // And back to the file.
    assert_eq!(vault.migrate_key(VaultKeyStorage::File).unwrap(), file);
    assert!(!vault.dir().join(KEYREF_FILE).exists());
    assert_eq!(keychain.get(&key::account(vault.dir())).unwrap(), None);
    assert_eq!(vault.list().unwrap().len(), 1);
}

#[test]
fn a_new_keychain_vault_without_a_keychain_keeps_its_key_in_the_file() {
    let profile = temp();
    let vault = Vault::of(profile.path(), Arc::new(Unreachable));
    vault
        .add(login("GitHub", "pw"), VaultKeyStorage::Keychain)
        .unwrap();
    assert!(vault.dir().join(KEY_FILE).exists());
    assert!(!vault.dir().join(KEYREF_FILE).exists());
    assert!(!vault.keychain_reachable());
    let keychain = Arc::new(MemoryKeychain::default());
    let other = temp();
    let in_keychain = Vault::of(other.path(), Arc::clone(&keychain) as _);
    in_keychain
        .add(login("GitHub", "pw"), VaultKeyStorage::Keychain)
        .unwrap();
    assert!(in_keychain.dir().join(KEYREF_FILE).exists());
    assert!(!in_keychain.dir().join(KEY_FILE).exists());
}

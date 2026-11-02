use super::{Managed, Owner};

fn dir_with(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(super::MANAGED_FILE), config).unwrap();
    dir
}

#[test]
fn values_and_locked_keys_are_read_and_validated() {
    let dir = dir_with("locked = [\"approvals.mode\"]\n\n[approvals]\nmode = \"manual\"\n");
    let managed = Managed::load(dir.path(), Owner::CurrentUser).unwrap();
    assert!(managed.config_loaded());
    assert_eq!(managed.locked(), ["approvals.mode"]);
    assert_eq!(managed.values()[0].0, "approvals.mode");
    assert!(managed.locks("approvals.mode"));
    assert!(managed.locks("approvals"), "the section holds a locked key");
    assert!(!managed.locks("agent.max_iterations"));
}

#[test]
fn an_invalid_layer_is_refused_naming_the_file() {
    let dir = dir_with("[approvals]\nmode = \"loud\"\n");
    let err = Managed::load(dir.path(), Owner::CurrentUser).unwrap_err();
    assert_eq!(err.code().code, "config.managed_invalid");
    assert!(err.message().contains("managed.toml"), "{}", err.message());
    let dir = dir_with("locked = [\"approvals.nope\"]\n");
    let err = Managed::load(dir.path(), Owner::CurrentUser).unwrap_err();
    assert!(
        err.message().contains("unknown key 'approvals.nope'"),
        "{}",
        err.message()
    );
}

#[test]
fn an_absent_layer_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let managed = Managed::load(dir.path(), Owner::Root).unwrap();
    assert!(!managed.config_loaded() && !managed.secrets_loaded());
}

#[cfg(unix)]
#[test]
fn managed_secrets_need_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(super::MANAGED_SECRETS_FILE);
    std::fs::write(&path, "OPENAI_API_KEY = \"sk-managed-123456\"\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = Managed::load(dir.path(), Owner::CurrentUser).unwrap_err();
    assert!(
        err.message().contains("must be 0600 or 0640"),
        "{}",
        err.message()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let managed = Managed::load(dir.path(), Owner::CurrentUser).unwrap();
    assert_eq!(managed.secret_names(), ["OPENAI_API_KEY"]);
    let err = Managed::load(dir.path(), Owner::Root).unwrap_err();
    assert!(err.message().contains("owned by root"), "{}", err.message());
}

/// The managed directory is the machine's `ProgramData` folder, whatever
/// the user's environment says.
#[cfg(windows)]
#[test]
fn the_windows_managed_dir_is_the_machine_program_data_folder() {
    let dir = super::platform_dir();
    assert!(dir.is_absolute(), "{}", dir.display());
    assert_eq!(dir.file_name().unwrap(), "Prometheus");
    let from_env = std::env::var_os("ProgramData").map(std::path::PathBuf::from);
    if let Some(from_env) = from_env {
        // On a stock machine both name the same folder.
        assert_eq!(
            dir.parent().unwrap().as_os_str().to_ascii_lowercase(),
            from_env.as_os_str().to_ascii_lowercase()
        );
    }
}

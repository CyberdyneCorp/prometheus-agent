//! One process serving two profiles resolves each session's terminal
//! backend from that session's own profile (spec: execution-environments,
//! Per-profile terminal backends in a shared gateway).
#![cfg(unix)] // the fake container runtime is a shell script

mod common;

use std::fs;
use std::io::Write as _;
use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_sandbox::backend::{BackendRegistry, Isolation};
use prometheus::chat::session::backend;

use common::temp_home;

/// A `docker` that only answers the rootless probe: selection never
/// starts a container (that happens on the first command).
const FAKE_DOCKER: &str = "#!/bin/sh\n[ \"$1\" = info ] && echo '[\"name=rootless\"]|Ubuntu'\n";

fn fake_docker(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join("docker");
    fs::write(&program, FAKE_DOCKER).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

/// Opens profile `name` of `home` after appending `config` to it.
fn profile(home: &Home, name: &str, config: &str) -> ProfileScope {
    let dir = home.profile(&ProfileName::parse(name).unwrap()).unwrap();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join("config.toml"))
        .unwrap();
    file.write_all(config.as_bytes()).unwrap();
    ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap()
}

/// Scenario: Two profiles, two backends.
#[test]
fn two_profiles_in_one_process_each_get_their_own_backend() {
    let root = temp_home();
    let runtime = fake_docker(root.path());
    let home = Home::open(root.path()).unwrap();
    let a = profile(
        &home,
        "a",
        &format!(
            "\n[terminal]\nbackend = \"container\"\n\n[terminal.container]\nruntime = \"{}\"\n",
            runtime.display()
        ),
    );
    let b = profile(&home, "b", "\n[terminal]\nbackend = \"local\"\n");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let registry = BackendRegistry::builtin();

    // `a` first, so a backend leaking from one scope to the next would show.
    let on_a = backend::select(&a, &registry, "session-a", &workspace, None)
        .expect("profile a runs on its container");
    assert_eq!(on_a.target.descriptor().isolation, Isolation::Container);
    assert_eq!(
        on_a.target.check(),
        Ok(()),
        "the container target is usable"
    );
    assert!(on_a.container.is_some());

    // `None`: the session keeps the local OS-sandbox target.
    assert!(
        backend::select(&b, &registry, "session-b", &workspace, None).is_none(),
        "profile b's terminal must run on its local backend"
    );

    // And `a` still gets its container after `b` was served.
    let again = backend::select(&a, &registry, "session-a2", &workspace, None).unwrap();
    assert_eq!(again.target.descriptor().isolation, Isolation::Container);
}

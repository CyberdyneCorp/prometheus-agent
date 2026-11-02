//! Profile creation options, alias launchers and portable exports (spec:
//! configuration-and-profiles, Profile creation options; Profile alias
//! launchers; Profile export excludes credentials by default).

mod common;

use std::path::Path;

use common::{
    mock_home, ok, run, run_cmd_with_stdin, run_with_stdin, stderr, temp_home, user_home,
};

fn launcher(home: &Path, alias: &str) -> std::path::PathBuf {
    let dir = user_home(home).join(".local").join("bin");
    if cfg!(windows) {
        dir.join(format!("{alias}.cmd"))
    } else {
        dir.join(alias)
    }
}

/// Scenario: Alias collision.
#[cfg(unix)]
#[test]
fn an_alias_shadowing_an_executable_on_path_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = temp_home();
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    let tools = home.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let git = tools.join("git");
    std::fs::write(&git, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = common::command(home.path());
    cmd.env("PATH", &tools)
        .args(["profile", "alias", "work", "--as", "git"]);
    let out = run_cmd_with_stdin(&mut cmd, "");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "alias 'git' would shadow the existing executable {}",
            git.display()
        )),
        "{}",
        stderr(&out)
    );
    assert!(!launcher(home.path(), "git").exists());
}

#[test]
fn reserved_names_and_subcommands_are_refused() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    for (alias, why) in [
        ("tmp", "alias 'tmp' is reserved"),
        ("chat", "alias 'chat' is a prometheus subcommand"),
        ("Bad Name", "is not a valid name"),
    ] {
        let out = run(home.path(), &["profile", "alias", "work", "--as", alias]);
        assert_eq!(out.status.code(), Some(2), "{alias}: {}", stderr(&out));
        assert!(stderr(&out).contains(why), "{}", stderr(&out));
    }
}

#[cfg(unix)]
#[test]
fn the_launcher_runs_prometheus_with_the_profile() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    assert!(!launcher(home.path(), "work").exists(), "--no-alias");
    ok(&run(
        home.path(),
        &["profile", "alias", "work", "--as", "wk"],
    ));
    let path = launcher(home.path(), "wk");
    // The profile lives outside the default home: the launcher opens it.
    let text = std::fs::read_to_string(&path).unwrap();
    let expected = format!(
        "exec prometheus --home '{}' -p work \"$@\"\n",
        home.path().display()
    );
    assert!(text.ends_with(&expected), "{text}");
    // Writing it again is fine: it is ours.
    ok(&run(
        home.path(),
        &["profile", "alias", "work", "--as", "wk"],
    ));
    let bin = Path::new(common::BIN).parent().unwrap();
    let out = std::process::Command::new(&path)
        .args(["config", "path"])
        .env("PATH", bin)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("profiles/work/config.toml"),
        "{out:?}"
    );
}

/// A profile of the default home gets a launcher without `--home`.
#[cfg(unix)]
#[test]
fn a_default_home_launcher_names_no_home() {
    let home = temp_home();
    let default = common::user_home(home.path()).join(".prometheus");
    let mut cmd = std::process::Command::new(common::BIN);
    cmd.env_remove("PROMETHEUS_HOME");
    common::isolate_user_home(&mut cmd, home.path());
    cmd.args(["--home"])
        .arg(&default)
        .args(["profile", "create", "work"]);
    ok(&cmd.stdin(std::process::Stdio::null()).output().unwrap());
    let text = std::fs::read_to_string(launcher(home.path(), "work")).unwrap();
    assert!(
        text.ends_with("\nexec prometheus -p work \"$@\"\n"),
        "{text}"
    );
}

#[test]
fn a_new_profile_gets_its_launcher_and_description() {
    let home = temp_home();
    let out = ok(&run(
        home.path(),
        &[
            "profile",
            "create",
            "coder",
            "--description",
            "for code reviews",
        ],
    ));
    let _ = out;
    assert!(launcher(home.path(), "coder").is_file());
    let shown = ok(&run(home.path(), &["profile", "show", "coder"]));
    assert!(shown.contains("description: for code reviews"), "{shown}");
    let listed = ok(&run(home.path(), &["profile", "list"]));
    assert!(listed.contains("coder  for code reviews"), "{listed}");
    let out = run(home.path(), &["profile", "create", "x", "--clone-secrets"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// Scenario: Channels not cloned by default.
#[test]
fn clone_all_leaves_messaging_channels_behind() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["profile", "create", "work", "--no-alias"],
    ));
    ok(&run(
        home.path(),
        &[
            "-p",
            "work",
            "config",
            "set",
            "gateway.platforms.telegram.token_secret",
            "TELEGRAM_BOT_TOKEN",
        ],
    ));
    let pairing = home.path().join("profiles/work/pairing");
    std::fs::create_dir_all(&pairing).unwrap();
    std::fs::write(pairing.join("telegram.json"), "{}").unwrap();
    ok(&run(
        home.path(),
        &[
            "-p",
            "work",
            "profile",
            "create",
            "work2",
            "--clone-all",
            "--no-alias",
        ],
    ));
    let config = std::fs::read_to_string(home.path().join("profiles/work2/config.toml")).unwrap();
    assert!(!config.contains("telegram"), "{config}");
    assert!(!home.path().join("profiles/work2/pairing").exists());
    ok(&run(
        home.path(),
        &[
            "profile",
            "create",
            "work3",
            "--clone-from",
            "work",
            "--clone-channels",
            "--no-alias",
        ],
    ));
    let config = std::fs::read_to_string(home.path().join("profiles/work3/config.toml")).unwrap();
    assert!(config.contains("[gateway.platforms.telegram]"), "{config}");
}

fn members(archive: &Path) -> Vec<String> {
    let file = std::fs::File::open(archive).unwrap();
    let mut tar = tar::Archive::new(zstd::Decoder::new(file).unwrap());
    tar.entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// Scenario: Default export has no secrets.
#[test]
fn a_default_export_holds_no_secrets_and_no_sessions() {
    let home = mock_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-test-0123456789abcdef",
    ));
    std::fs::write(home.path().join("profiles/default/SOUL.md"), "I am me\n").unwrap();
    ok(&run(home.path(), &["-z", "remember the session"]));
    let archive = home.path().join("default.tar.zst");
    ok(&run(
        home.path(),
        &[
            "profile",
            "export",
            "default",
            "-o",
            archive.to_str().unwrap(),
        ],
    ));
    let names = members(&archive);
    assert!(names.contains(&"config.toml".to_owned()), "{names:?}");
    assert!(names.contains(&"SOUL.md".to_owned()), "{names:?}");
    for forbidden in ["secrets.toml", "state.db", "secrets.sealed"] {
        assert!(!names.iter().any(|n| n.ends_with(forbidden)), "{names:?}");
    }
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("blobs/") || n.starts_with("logs/"))
    );
    let bytes = std::fs::read(&archive).unwrap();
    let text = String::from_utf8_lossy(&zstd::decode_all(bytes.as_slice()).unwrap()).into_owned();
    assert!(!text.contains("sk-test-0123456789abcdef"));
}

#[test]
fn an_export_with_sessions_and_secrets_round_trips_under_its_passphrase() {
    let home = mock_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "OPENAI_API_KEY"],
        "sk-test-0123456789abcdef",
    ));
    ok(&run(home.path(), &["-z", "remember the session"]));
    let archive = home.path().join("full.tar.zst");
    let archive_arg = archive.to_str().unwrap();
    ok(&run_with_stdin(
        home.path(),
        &[
            "profile",
            "export",
            "default",
            "-o",
            archive_arg,
            "--with-sessions",
            "--with-secrets",
        ],
        "a long passphrase",
    ));
    let names = members(&archive);
    assert!(names.contains(&"state.db".to_owned()) && names.contains(&"secrets.sealed".to_owned()));
    let raw = zstd::decode_all(std::fs::read(&archive).unwrap().as_slice()).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("sk-test-0123456789abcdef"));

    let wrong = run_with_stdin(
        home.path(),
        &["profile", "import", archive_arg, "--as", "wrong"],
        "another passphrase",
    );
    assert_eq!(wrong.status.code(), Some(78), "{}", stderr(&wrong));
    assert!(
        !home.path().join("profiles/wrong").exists(),
        "nothing imported"
    );

    ok(&run_with_stdin(
        home.path(),
        &["profile", "import", archive_arg, "--as", "copy"],
        "a long passphrase",
    ));
    let listed = ok(&run(
        home.path(),
        &["-p", "copy", "--json", "secrets", "list"],
    ));
    assert!(listed.contains("OPENAI_API_KEY"), "{listed}");
    let sessions = ok(&run(
        home.path(),
        &["-p", "copy", "--json", "sessions", "list", "--all"],
    ));
    let sessions: serde_json::Value = serde_json::from_str(&sessions).unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);
}

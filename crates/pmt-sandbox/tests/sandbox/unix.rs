//! Confinement on Linux and macOS, run for real (spec: security-boundaries,
//! OS sandbox per platform; Default sandbox denies credential stores;
//! execution-environments, Local backend runs under the OS sandbox).

use std::path::Path;
use std::process::Output;

use pmt_sandbox::{Enforcement, Sandbox};

use crate::common::{Fixture, SECRET, confine};

fn run(sandbox: &Sandbox, program: &str, args: &[&Path]) -> Output {
    sandbox
        .command(program)
        .unwrap()
        .args(args)
        .output()
        .unwrap()
}

fn cat(sandbox: &Sandbox, path: &Path) -> Output {
    run(sandbox, "/bin/cat", &[path])
}

fn touch(sandbox: &Sandbox, path: &Path) -> Output {
    run(sandbox, "/usr/bin/touch", &[path])
}

fn assert_denied(output: &Output) {
    assert!(!output.status.success(), "expected a denial: {output:?}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
}

#[test]
fn host_sandbox_is_full() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    assert_eq!(
        sandbox.report().enforcement(),
        Enforcement::Full,
        "{:?}",
        sandbox.report()
    );
}

#[test]
fn reading_ssh_keys_is_denied_with_the_default_capabilities() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    assert_denied(&cat(&sandbox, &fixture.home.join(".ssh/id_rsa")));
    assert_denied(&cat(&sandbox, &fixture.home.join(".ssh/id_ed25519")));
}

#[test]
fn broad_read_grant_still_denies_credential_stores() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&["fs.read:~/**"]));
    let notes = cat(&sandbox, &fixture.home.join("notes.txt"));
    assert!(notes.status.success(), "{notes:?}");
    assert_eq!(notes.stdout, b"public notes");
    for store in [".aws/credentials", ".ssh/id_rsa", ".prometheus/config.toml"] {
        assert_denied(&cat(&sandbox, &fixture.home.join(store)));
    }
}

#[test]
fn a_grant_naming_the_exact_path_is_honoured() {
    let fixture = Fixture::new();
    let credentials = fixture.home.join(".aws/credentials");
    let grant = format!("fs.read:{}", credentials.display());
    let sandbox = confine(fixture.policy(&["fs.read:~/**", &grant]));
    let output = cat(&sandbox, &credentials);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn writes_outside_the_workspace_fail() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&["fs.read:~/**"]));
    let inside = fixture.work.join("made-here");
    assert!(touch(&sandbox, &inside).status.success());
    assert!(inside.exists());

    let bashrc = fixture.home.join(".bashrc");
    let outside = fixture.outside.join("escaped");
    for target in [&bashrc, &outside] {
        let output = touch(&sandbox, target);
        assert!(!output.status.success(), "{output:?}");
        assert!(!target.exists(), "{} was created", target.display());
    }
    let scratch = fixture.paths.scratch[0].join("scratch-file");
    assert!(
        touch(&sandbox, &scratch).status.success(),
        "own scratch stays writable"
    );
}

#[test]
fn starting_processes_needs_process_spawn() {
    let fixture = Fixture::new();
    let script = "/usr/bin/true && echo ran";
    let without = pmt_sandbox::Policy::from_capabilities(
        &["fs.read:<toolchain>", "fs.write:<workspace>/**"],
        &fixture.paths,
    )
    .unwrap();
    let denied = confine(without)
        .command("/bin/sh")
        .unwrap()
        .args(["-c", script])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&denied.stdout).contains("ran"),
        "{denied:?}"
    );

    let allowed = confine(fixture.policy(&[]));
    let output = allowed
        .command("/bin/sh")
        .unwrap()
        .args(["-c", script])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "ran",
        "{output:?}"
    );
}

#[test]
fn programs_outside_granted_roots_cannot_run() {
    let fixture = Fixture::new();
    let tool = fixture.outside.join("tool");
    std::fs::copy("/bin/echo", &tool).unwrap();
    let sandbox = confine(fixture.policy(&[]));
    let ran = sandbox.command(&tool).unwrap().arg("hi").output();
    assert!(
        ran.as_ref().map_or(true, |o| !o.status.success()),
        "{ran:?}"
    );
}

#[test]
fn tcp_connections_need_net_connect() {
    let fixture = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = format!("echo hi > /dev/tcp/127.0.0.1/{port} && echo connected");
    let connect = |sandbox: &Sandbox| {
        sandbox
            .command("/bin/bash")
            .map(|mut c| c.args(["-c", &script]).output().unwrap())
    };

    let offline = pmt_sandbox::Policy::from_capabilities(
        &[
            "fs.read:<toolchain>",
            "fs.write:<workspace>/**",
            "process.spawn:*",
        ],
        &fixture.paths,
    )
    .unwrap();
    let offline = confine(offline);
    match offline.report().level(pmt_sandbox::Feature::Net) {
        pmt_sandbox::Level::Enforced => {
            let output = connect(&offline).unwrap();
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("connected"),
                "{output:?}"
            );
        }
        // A kernel without Landlock TCP rules: the command is refused.
        _ => assert_eq!(connect(&offline).unwrap_err().exit_code(), 77),
    }

    let online = confine(fixture.policy(&[]));
    let output = connect(&online).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "connected",
        "{output:?}"
    );
}

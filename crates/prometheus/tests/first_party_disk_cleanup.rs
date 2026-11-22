//! The embedded `disk-cleanup` plugin (spec: plugin-system, First-party
//! disk-cleanup plugin), hosted by the python bridge for real. Needs uv on
//! PATH (see `pmt_pybridge::testkit`).

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{command, mock_home, ok, run, stderr, stdout};

/// `<home>/tmp`: the run's system temporary directory (`TMPDIR`), so the
/// plugin's `<tmp>/prometheus-*` root stays inside the test home.
fn tmp(home: &Path) -> PathBuf {
    let dir = home.join("tmp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

fn prometheus(home: &Path) -> std::process::Command {
    let mut cmd = command(home);
    cmd.env("TMPDIR", tmp(home)).stdin(Stdio::null());
    cmd
}

fn enable(home: &Path) {
    pmt_pybridge::testkit::seed(home);
    let out = prometheus(home)
        .args(["plugins", "enable", "disk-cleanup", "--grant", "all"])
        .output()
        .unwrap();
    ok(&out);
}

/// Runs one turn in `cwd` whose model writes each of `paths`.
fn write_files(home: &Path, cwd: &Path, paths: &[String]) -> std::process::Output {
    let calls: Vec<String> = paths
        .iter()
        .map(|p| {
            format!(
                "[[rule.tool_calls]]\nname = \"write_file\"\narguments = {{ path = {}, content = \"x = 1\\n\" }}\n",
                toml::Value::String(p.clone())
            )
        })
        .collect();
    let script = home.join("script.toml");
    std::fs::write(
        &script,
        format!(
            "[[rule]]\nuser = \"go\"\n{}\n[[rule]]\ntool_result = \"\"\ntext = \"written\"\n",
            calls.join("\n")
        ),
    )
    .unwrap();
    prometheus(home)
        .current_dir(cwd)
        .args(["-z", "go", "--mock-script"])
        .arg(&script)
        .output()
        .unwrap()
}

fn tracked(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/plugin-data/disk-cleanup/tracked.json"))
        .unwrap_or_default()
}

#[test]
fn the_plugin_is_embedded_and_disabled_by_default() {
    let home = mock_home();
    let listing = ok(&run(home.path(), &["--json", "plugins", "list"]));
    let listing: serde_json::Value = serde_json::from_str(&listing).unwrap();
    let rows = listing["plugins"].as_array().unwrap();
    for id in ["disk-cleanup", "google_meet"] {
        let row = rows
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("{id} in {listing}"));
        assert_eq!(row["source"], "embedded", "{row}");
        assert_eq!(row["state"], "disabled", "{row}");
    }
}

#[test]
fn project_test_file_is_never_swept() {
    let home = mock_home();
    enable(home.path());
    let repo = home.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let out = write_files(home.path(), &repo, &["test_api.py".to_owned()]);
    assert_eq!(stdout(&out), "written\n", "{}", stderr(&out));
    assert!(repo.join("test_api.py").is_file(), "the project file stays");
    // The same name written in the plugin's scope (`<tmp>/prometheus-*`) is
    // tracked and swept when that session ends.
    let scratch = tmp(home.path()).join("prometheus-scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let out = write_files(home.path(), &scratch, &["test_api.py".to_owned()]);
    assert_eq!(stdout(&out), "written\n", "{}", stderr(&out));
    assert!(
        !scratch.join("test_api.py").exists(),
        "the scratch test file is swept"
    );
    assert!(
        repo.join("test_api.py").is_file(),
        "the project file still stays"
    );
    // A repository kept inside the scope is a user tree: never swept.
    let inner = scratch.join("project");
    std::fs::create_dir_all(inner.join(".git")).unwrap();
    let out = write_files(home.path(), &inner, &["test_api.py".to_owned()]);
    assert_eq!(stdout(&out), "written\n", "{}", stderr(&out));
    assert!(
        inner.join("test_api.py").is_file(),
        "the inner project file stays"
    );
    let log = std::fs::read_to_string(
        home.path()
            .join("profiles/default/plugin-data/disk-cleanup/cleanup.log"),
    )
    .unwrap();
    assert!(log.contains("prometheus-scratch/test_api.py"), "{log}");
    assert!(!log.contains("repo/test_api.py"), "{log}");
    assert!(!log.contains("project/test_api.py"), "{log}");
    assert!(!tracked(home.path()).contains("test_api.py"));
}

/// `/disk-cleanup` in the line interface (a pseudo-terminal, Unix only).
#[cfg(unix)]
#[test]
fn the_slash_command_tracks_and_forgets_by_hand() {
    use common::pty::{exit, prompt, spawn_with};
    use expectrl::Expect as _;
    let home = mock_home();
    enable(home.path());
    let report = tmp(home.path()).join("prometheus-run/report.txt");
    std::fs::create_dir_all(report.parent().unwrap()).unwrap();
    std::fs::write(&report, "r").unwrap();
    let kept = home.path().join("profiles/default/logs/test_kept.log");
    std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
    std::fs::write(&kept, "l").unwrap();
    let tmp_dir = tmp(home.path());
    let mut p = spawn_with(home.path(), &[], &[("TMPDIR", tmp_dir.to_str().unwrap())]);
    prompt(&mut p);
    let mut slash = |line: String, expected: &str| {
        p.send_line(format!("/disk-cleanup {line}")).unwrap();
        p.expect(expected).unwrap();
        prompt(&mut p);
    };
    slash(
        format!("track {} research", report.display()),
        "as 'research'",
    );
    slash(
        format!("track {} temp", kept.display()),
        "protected directory",
    );
    slash("status".to_owned(), "research");
    slash("dry-run".to_owned(), "Deleted automatically: 0 files");
    slash(format!("forget {}", report.display()), "Stopped tracking");
    exit(p);
    assert!(report.is_file() && kept.is_file(), "nothing deleted");
    assert!(!tracked(home.path()).contains("report.txt"));
}

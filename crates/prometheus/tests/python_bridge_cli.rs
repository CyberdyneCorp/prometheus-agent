//! `prometheus bridge` (spec: python-bridge, Bridge diagnostics; Named side
//! environments; Per-plugin pinned environments). Needs uv on PATH (see
//! `pmt_pybridge::testkit`).

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use self::common::{command, mock_home, ok, run, stderr, stdout};

fn with_index(home: &Path, args: &[&str], url: &str) -> std::process::Output {
    command(home)
        .args(args)
        .env("UV_DEFAULT_INDEX", url)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn a_named_environment_prints_only_its_path_and_is_reused_offline() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let index = home.path().join("index");
    pmt_pybridge::testkit::index(
        &index,
        &[(
            "pillow",
            "11.0.0",
            &[("PIL/__init__.py", "VERSION = '11.0.0'\n")],
        )],
    );
    let served = pmt_pybridge::testkit::serve(&index);
    let first = with_index(
        home.path(),
        &["bridge", "env", "ocr", "--extra", "pillow>=10,<12"],
        &served.url,
    );
    assert!(first.status.success(), "{}", stderr(&first));
    let path = stdout(&first);
    assert_eq!(path.lines().count(), 1, "{path}");
    assert!(path.contains("/python/named/ocr-"), "{path}");
    // No index any more: the second run makes no request.
    let url = served.url.clone();
    drop(served);
    let second = with_index(
        home.path(),
        &["bridge", "env", "ocr", "--extra", "pillow>=10,<12"],
        &url,
    );
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(stdout(&second), path);
    let python = path.trim();
    let out = std::process::Command::new(python)
        .args(["-c", "import PIL; print(PIL.VERSION)"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "11.0.0");
    // Names and bounds are checked.
    let out = run(home.path(), &["bridge", "env", "Bad Name"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let out = run(
        home.path(),
        &["bridge", "env", "x", "--extra", "pillow>=10"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("has no upper bound"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn doctor_names_the_plugin_and_package_of_a_corrupted_environment() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let index = home.path().join("index");
    pmt_pybridge::testkit::index(
        &index,
        &[
            (
                "notes-sdk",
                "1.0.0",
                &[("notes_sdk/__init__.py", "VERSION = '1.0.0'\n")],
            ),
            (
                "notes-sdk",
                "1.1.0",
                &[("notes_sdk/__init__.py", "VERSION = '1.1.0'\n")],
            ),
        ],
    );
    let served = pmt_pybridge::testkit::serve(&index);
    let dir = pmt_pybridge::testkit::place(home.path(), "lazy", "notes");
    std::fs::write(dir.join("requirements.txt"), "notes-sdk==1.0.0\n").unwrap();
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "notes", "--grant", "all"],
    ));
    let out = with_index(home.path(), &["plugins", "update", "notes"], &served.url);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run(home.path(), &["bridge", "doctor"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("✓ notes: environment matches bridge.lock"),
        "{}",
        stdout(&out)
    );
    // Something installs another version behind the lock's back.
    let env = std::fs::read_dir(home.path().join("profiles/default/python/envs"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("notes-"))
        .unwrap()
        .path();
    let uv = pmt_pybridge::runtime::Uv::of(home.path()).unwrap();
    let status = uv
        .command()
        .args([
            "pip",
            "install",
            "--quiet",
            "--index-url",
            &served.url,
            "notes-sdk==1.1.0",
            "--python",
        ])
        .arg(env.join("bin/python"))
        .status()
        .unwrap();
    assert!(status.success());
    let out = run(home.path(), &["bridge", "doctor"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("✗ notes: package notes-sdk is 1.1.0 but bridge.lock pins 1.0.0"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn status_shows_a_running_sidecar_and_logs_show_its_life() {
    let home = mock_home();
    pmt_pybridge::testkit::seed(home.path());
    let dir = pmt_pybridge::testkit::place(home.path(), "echo", "echo");
    ok(&run(
        home.path(),
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(
        home.path(),
        &["plugins", "enable", "echo", "--grant", "all"],
    ));
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        "[[rule]]\nuser = \"go\"\n[[rule.tool_calls]]\nname = \"slow\"\narguments = { seconds = 4 }\n\n[[rule]]\ntool_result = \"done\"\ntext = \"slept\"\n",
    )
    .unwrap();
    let mut chat = command(home.path())
        .args(["-z", "go", "--mock-script", script.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    while Instant::now() < deadline {
        seen = ok(&run(home.path(), &["--json", "bridge", "status"]));
        if seen.contains("\"state\":\"running\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen.contains("prometheus.bridge_status.v1"), "{seen}");
    assert!(
        seen.contains("\"plugin\":\"echo\"") && seen.contains("\"state\":\"running\""),
        "{seen}"
    );
    assert!(chat.wait().unwrap().success());
    let logs = ok(&run(home.path(), &["bridge", "logs", "echo"]));
    assert!(logs.contains("[bridge] started (pid"), "{logs}");
    let out = ok(&run(home.path(), &["bridge", "status"]));
    assert_eq!(out, "no bridge sidecars are running for this profile\n");
}

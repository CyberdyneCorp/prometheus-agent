//! `prometheus desktop` (spec: desktop-app, Launching the desktop from the
//! CLI): launching the installed app with the new-chat link, the missing
//! app, and `--install` against a local release origin whose packages are
//! signed with the test key debug builds trust.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::release::{Fixture, code, sign};
use common::{run, stderr, user_home};

/// Where `--install` puts the app (and `desktop` looks first).
fn install_target(home: &Path) -> PathBuf {
    let user = user_home(home);
    if cfg!(target_os = "macos") {
        user.join("Applications/Prometheus.app")
    } else {
        user.join(".local/share/prometheus-desktop/Prometheus.AppImage")
    }
}

fn executable(target: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        target.join("Contents/MacOS/prometheus-desktop")
    } else {
        target.to_path_buf()
    }
}

/// A fake installed app that writes its arguments next to itself.
fn fake_app(home: &Path) -> PathBuf {
    let exe = executable(&install_target(home));
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", path.display());
}

#[test]
fn without_an_app_it_exits_69_naming_the_install_command() {
    let home = common::temp_home();
    let out = run(home.path(), &["desktop"]);
    assert_eq!(code(&out), 69, "{}", stderr(&out));
    assert!(
        stderr(&out)
            .contains("Prometheus Desktop is not installed; run 'prometheus desktop --install'"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Launch in a project directory (the CLI side; the window side
/// is `desktop_background.rs`).
#[test]
fn launching_in_a_project_directory_passes_the_new_chat_link() {
    let home = common::temp_home();
    let exe = fake_app(home.path());
    let project = home.path().join("src/api");
    std::fs::create_dir_all(&project).unwrap();
    let out = run(home.path(), &["gui", "--cwd", project.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let mut args_file = exe.into_os_string();
    args_file.push(".args");
    let args = wait_for(Path::new(&args_file));
    let link = args.lines().next().unwrap();
    assert_eq!(
        pmt_desktop::deeplink::parse(link),
        Some(pmt_desktop::deeplink::DeepLink::New {
            profile: Some("default".into()),
            cwd: Some(
                std::path::absolute(&project)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ),
        }),
        "{link}"
    );
    assert!(!args.contains("--ignore-existing"));
}

#[test]
fn ignore_existing_is_passed_to_the_app() {
    let home = common::temp_home();
    let exe = fake_app(home.path());
    let out = run(home.path(), &["desktop", "--ignore-existing"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let mut args_file = exe.clone().into_os_string();
    args_file.push(".args");
    let args = wait_for(Path::new(&args_file));
    assert!(args.lines().any(|a| a == "--ignore-existing"), "{args}");
}

/// The desktop package of `target` with `marker` as its app.
fn package(target: &str, marker: &[u8]) -> (String, Vec<u8>) {
    let name = if target.ends_with("apple-darwin") {
        "prometheus-desktop-universal2-apple-darwin.app.tar.gz".to_owned()
    } else {
        format!(
            "prometheus-desktop-{}-linux.AppImage",
            target.split('-').next().unwrap()
        )
    };
    if !target.ends_with("apple-darwin") {
        return (name, marker.to_vec());
    }
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(marker.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(
        &mut header,
        "Prometheus.app/Contents/MacOS/prometheus-desktop",
        marker,
    )
    .unwrap();
    (name, tar.into_inner().unwrap().finish().unwrap())
}

/// Scenario: Tampered download.
#[test]
fn a_tampered_desktop_download_installs_nothing_and_exits_77() {
    let f = Fixture::self_install();
    let (name, bytes) = package(&f.target, b"#!/bin/sh\necho desktop\n");
    let path = format!("/v{}/{name}", f.version.to_string().trim_start_matches('v'));
    f.mount(&format!("{path}.minisig"), sign(&bytes).into_bytes());
    let mut tampered = bytes.clone();
    tampered.extend_from_slice(b"\n# not what was signed\n");
    f.mount(&path, tampered);
    let out = f.run(&["desktop", "--install"]);
    assert_eq!(code(&out), 77, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("signature verification failed"),
        "{}",
        stderr(&out)
    );
    assert!(!install_target(&f.home).exists(), "nothing was installed");
    assert!(
        !f.home.join("updates/desktop").join(&name).exists(),
        "the download was not kept"
    );
}

#[test]
fn a_verified_desktop_download_is_installed_and_launches() {
    let f = Fixture::self_install();
    let marker = b"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n";
    let (name, bytes) = package(&f.target, marker);
    let path = format!("/v{}/{name}", f.version.to_string().trim_start_matches('v'));
    f.mount(&format!("{path}.minisig"), sign(&bytes).into_bytes());
    f.mount(&path, bytes);
    let out = f.run(&["desktop", "--install"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let exe = executable(&install_target(&f.home));
    assert_eq!(std::fs::read(&exe).unwrap(), marker);
    let out = f.run(&["desktop"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let mut args_file = exe.into_os_string();
    args_file.push(".args");
    assert!(wait_for(Path::new(&args_file)).starts_with("prometheus://new?profile=default"));
}

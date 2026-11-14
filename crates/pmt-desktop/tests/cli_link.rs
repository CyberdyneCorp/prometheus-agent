//! The "Install command-line tool" action (spec: distribution-and-update,
//! Desktop packages and signing): which installs offer it, the link it
//! makes, what it leaves alone, and the `AppImage` started through it.
#![cfg(unix)]

use std::ffi::OsStr;
use std::path::Path;

use pmt_desktop::cli_link::{install, link_target, runs_cli};

#[test]
fn only_the_app_bundle_and_the_appimage_offer_the_action() {
    let bundled = Path::new("/Applications/Prometheus.app/Contents/MacOS/prometheus");
    assert_eq!(
        link_target(
            Path::new("/Applications/Prometheus.app/Contents/MacOS/prometheus-desktop"),
            None,
            bundled
        ),
        Some(bundled.to_owned()),
        "macOS links the CLI inside the bundle"
    );
    let image = Path::new("/home/u/Apps/Prometheus.AppImage");
    assert_eq!(
        link_target(
            Path::new("/tmp/.mount_ab12/usr/bin/prometheus-desktop"),
            Some(image),
            Path::new("/tmp/.mount_ab12/usr/bin/prometheus")
        ),
        Some(image.to_owned()),
        "the AppImage's mount point changes on every start: link the image"
    );
    assert_eq!(
        link_target(
            Path::new("/usr/bin/prometheus-desktop"),
            None,
            Path::new("/usr/bin/prometheus")
        ),
        None,
        "deb and rpm put /usr/bin/prometheus on PATH themselves"
    );
}

#[test]
fn the_link_points_at_the_bundled_cli_and_reports_path() {
    let home = tempfile::tempdir().unwrap();
    let cli = home.path().join("Prometheus.app/Contents/MacOS/prometheus");
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&cli, "#!/bin/sh\n").unwrap();
    let bin = home.path().join(".local/bin");
    let path = std::env::join_paths([Path::new("/usr/bin"), &bin]).unwrap();
    let installed = install(home.path(), &cli, Some(&path)).unwrap();
    assert_eq!(installed.link, bin.join("prometheus"));
    assert_eq!(std::fs::read_link(&installed.link).unwrap(), cli);
    assert!(installed.on_path);
    // Again (the app moved, say): the app's own link is replaced.
    let moved = home
        .path()
        .join("Apps/Prometheus.app/Contents/MacOS/prometheus");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::write(&moved, "#!/bin/sh\n").unwrap();
    std::fs::remove_file(&cli).unwrap();
    let again = install(home.path(), &moved, Some(OsStr::new("/usr/bin"))).unwrap();
    assert_eq!(std::fs::read_link(&again.link).unwrap(), moved);
    assert!(!again.on_path, "~/.local/bin is not on this PATH");
}

#[test]
fn another_install_of_the_cli_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    // install.sh's binary.
    std::fs::write(bin.join("prometheus"), "self-installed").unwrap();
    let target = home.path().join("Prometheus.AppImage");
    std::fs::write(&target, "image").unwrap();
    let refused = install(home.path(), &target, None).unwrap_err();
    assert!(refused.contains("another install"), "{refused}");
    assert_eq!(
        std::fs::read_to_string(bin.join("prometheus")).unwrap(),
        "self-installed"
    );
    // A link of the user's own to something that exists stays too.
    std::fs::remove_file(bin.join("prometheus")).unwrap();
    let theirs = home.path().join("their-prometheus");
    std::fs::write(&theirs, "theirs").unwrap();
    std::os::unix::fs::symlink(&theirs, bin.join("prometheus")).unwrap();
    assert!(install(home.path(), &target, None).is_err());
    assert_eq!(std::fs::read_link(bin.join("prometheus")).unwrap(), theirs);
}

#[test]
fn the_appimage_runs_the_cli_when_started_as_prometheus_or_pmt() {
    assert!(runs_cli(Some(OsStr::new("/home/u/.local/bin/prometheus"))));
    assert!(runs_cli(Some(OsStr::new("pmt"))));
    assert!(!runs_cli(Some(OsStr::new(
        "/home/u/Apps/Prometheus.AppImage"
    ))));
    assert!(!runs_cli(None), "started from the file manager");
}

/// A machine whose app is a macOS bundle: the action links its CLI.
struct Bundle(std::path::PathBuf);

impl pmt_desktop::Native for Bundle {
    fn open_url(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
    fn open_path(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn pick(&self, _directory: bool, _title: Option<&str>) -> Option<std::path::PathBuf> {
        None
    }
    fn notify(&self, _title: &str, _body: &str) -> Result<(), String> {
        Ok(())
    }
    fn set_badge(&self, _window: &str, _count: Option<u32>) -> Result<(), String> {
        Ok(())
    }
    fn register_hotkey(&self, _chord: &str) -> Result<(), String> {
        Ok(())
    }
    fn read_clipboard(&self) -> Result<String, String> {
        Ok(String::new())
    }
    fn cli_link_target(&self) -> Option<std::path::PathBuf> {
        Some(self.0.clone())
    }
}

fn shell(home: &Path, native: Option<Bundle>) -> pmt_desktop::Shell {
    let native: std::sync::Arc<dyn pmt_desktop::Native> = match native {
        Some(bundle) => std::sync::Arc::new(bundle),
        None => std::sync::Arc::new(NoAction),
    };
    pmt_desktop::Shell::new(
        pmt_desktop::ShellConfig {
            binary: home.join("prometheus"),
            home: home.join(".prometheus"),
            user_home: home.to_path_buf(),
            ensure_bound: std::time::Duration::from_secs(5),
        },
        native,
    )
}

/// A machine whose install puts the CLI on PATH itself (deb, MSIX).
struct NoAction;

impl pmt_desktop::Native for NoAction {
    fn open_url(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
    fn open_path(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn pick(&self, _directory: bool, _title: Option<&str>) -> Option<std::path::PathBuf> {
        None
    }
    fn notify(&self, _title: &str, _body: &str) -> Result<(), String> {
        Ok(())
    }
    fn set_badge(&self, _window: &str, _count: Option<u32>) -> Result<(), String> {
        Ok(())
    }
    fn register_hotkey(&self, _chord: &str) -> Result<(), String> {
        Ok(())
    }
    fn read_clipboard(&self) -> Result<String, String> {
        Ok(String::new())
    }
}

#[tokio::test]
async fn settings_offer_the_action_only_where_it_applies() {
    let home = tempfile::tempdir().unwrap();
    let cli = home.path().join("Prometheus.app/Contents/MacOS/prometheus");
    let bundle = shell(home.path(), Some(Bundle(cli.clone())));
    let options = bundle
        .handle("main", pmt_desktop::Request::LaunchOptions)
        .await
        .unwrap();
    assert_eq!(options["cli_link"], true);
    let answer = bundle
        .handle("main", pmt_desktop::Request::InstallCli)
        .await
        .unwrap();
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(
        std::fs::read_link(home.path().join(".local/bin/prometheus")).unwrap(),
        cli
    );
    let other = tempfile::tempdir().unwrap();
    let package = shell(other.path(), None);
    let options = package
        .handle("main", pmt_desktop::Request::LaunchOptions)
        .await
        .unwrap();
    assert_eq!(options["cli_link"], false);
    let answer = package
        .handle("main", pmt_desktop::Request::InstallCli)
        .await
        .unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(!other.path().join(".local/bin/prometheus").exists());
}

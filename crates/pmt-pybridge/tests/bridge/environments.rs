//! Pinned environments (spec: python-bridge, Per-plugin pinned
//! environments).

use std::path::Path;
use std::process::Command;

use pmt_pybridge::env;
use pmt_pybridge::runtime::{self, Uv};
use pmt_pybridge::testkit;

/// The managed uv of `home` resolving from the local index `url`.
fn uv_with(home: &Path, url: &str) -> Uv {
    let mut uv = Uv::of(home).unwrap();
    uv.env = vec![("UV_DEFAULT_INDEX".to_owned(), url.to_owned())];
    uv
}

fn imports(python: &Path, module: &str) -> String {
    let out = Command::new(python)
        .args(["-c", &format!("import {module}; print({module}.VERSION)")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn requirements_are_locked_to_one_exact_version_with_its_hash() {
    let home = tempfile::tempdir().unwrap();
    testkit::seed(home.path());
    let module = [("requests/__init__.py", "VERSION = 'x'\n")];
    testkit::index(
        &home.path().join("index"),
        &[
            ("requests", "2.31.0", &module),
            ("requests", "2.32.3", &module),
            ("requests", "3.0.0", &module),
        ],
    );
    let url = testkit::file_url(&home.path().join("index"));
    let python = runtime::require(home.path(), testkit::VERSION).unwrap();
    let requirements = vec!["requests>=2.31,<3".to_owned()];
    env::check_bounds(&requirements, false).unwrap();
    let lock = env::lock(
        &uv_with(home.path(), &url),
        &python,
        &requirements,
        &home.path().join("lock"),
    )
    .unwrap();
    let text = std::fs::read_to_string(&lock).unwrap();
    let pins: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("requests=="))
        .collect();
    assert_eq!(pins.len(), 1, "{text}");
    assert!(pins[0].starts_with("requests==2.32.3"), "{text}");
    assert!(text.contains("--hash=sha256:"), "{text}");
}

#[test]
fn an_environment_is_built_offline_from_the_cache_and_then_reused() {
    let home = tempfile::tempdir().unwrap();
    testkit::seed(home.path());
    let index = home.path().join("index");
    testkit::index(
        &index,
        &[(
            "notes-sdk",
            "1.0.0",
            &[("notes_sdk/__init__.py", "VERSION = '1.0.0'\n")],
        )],
    );
    let served = testkit::serve(&index);
    let python = runtime::require(home.path(), testkit::VERSION).unwrap();
    let uv = uv_with(home.path(), &served.url);
    let lock = env::lock(
        &uv,
        &python,
        &["notes-sdk==1.0.0".to_owned()],
        &home.path().join("lock"),
    )
    .unwrap();
    let root = home.path().join("envs");
    // Install time fills the package cache.
    let first = env::ensure(&uv, &python, &root, "notes", &lock, false).unwrap();
    assert_eq!(imports(&first.python, "notes_sdk"), "1.0.0");
    // The cache alone builds it again, offline, with the index gone.
    drop(served);
    std::fs::remove_dir_all(&index).unwrap();
    std::fs::remove_dir_all(&first.dir).unwrap();
    let rebuilt = env::ensure(&uv, &python, &root, "notes", &lock, true).unwrap();
    assert_eq!(rebuilt.dir, first.dir);
    assert!(
        rebuilt
            .dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("notes-")
    );
    assert_eq!(imports(&rebuilt.python, "notes_sdk"), "1.0.0");
    // A later session reuses it without running uv at all.
    let no_uv = Uv::new(home.path().join("no-such-uv"), home.path());
    let reused = env::ensure(&no_uv, &python, &root, "notes", &lock, true).unwrap();
    assert_eq!(reused.dir, first.dir);
}

#[test]
fn doctor_finds_a_package_that_no_longer_matches_the_lock() {
    let home = tempfile::tempdir().unwrap();
    testkit::seed(home.path());
    testkit::index(
        &home.path().join("index"),
        &[
            (
                "pinned",
                "1.0.0",
                &[("pinned/__init__.py", "VERSION = '1.0.0'\n")],
            ),
            (
                "pinned",
                "1.1.0",
                &[("pinned/__init__.py", "VERSION = '1.1.0'\n")],
            ),
        ],
    );
    let url = testkit::file_url(&home.path().join("index"));
    let python = runtime::require(home.path(), testkit::VERSION).unwrap();
    let uv = uv_with(home.path(), &url);
    let lock = env::lock(
        &uv,
        &python,
        &["pinned==1.0.0".to_owned()],
        &home.path().join("lock"),
    )
    .unwrap();
    let built = env::ensure(&uv, &python, &home.path().join("envs"), "p", &lock, false).unwrap();
    assert!(env::verify(&uv, &built, &lock).unwrap().is_empty());
    let status = uv
        .command()
        .args(["pip", "install", "--quiet", "pinned==1.1.0", "--python"])
        .arg(&built.python)
        .status()
        .unwrap();
    assert!(status.success());
    let mismatches = env::verify(&uv, &built, &lock).unwrap();
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].package, "pinned");
    assert_eq!(mismatches[0].installed.as_deref(), Some("1.1.0"));
}

/// Writes an sdist-only package `name` 1.0.0 to the index at `root`: its
/// in-tree build backend writes `marker` the moment it is imported.
#[cfg(unix)]
fn sdist_only(root: &Path, name: &str, marker: &Path) {
    let source = root.join("src").join(format!("{name}-1.0.0"));
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("pyproject.toml"),
        "[build-system]\nrequires = []\nbuild-backend = \"backend\"\nbackend-path = [\".\"]\n",
    )
    .unwrap();
    std::fs::write(
        source.join("backend.py"),
        format!(
            "import pathlib\npathlib.Path({:?}).write_text('built')\n",
            marker.display().to_string()
        ),
    )
    .unwrap();
    let files = root.join("files");
    std::fs::create_dir_all(&files).unwrap();
    let archive = format!("{name}-1.0.0.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(files.join(&archive))
        .arg("-C")
        .arg(root.join("src"))
        .arg(format!("{name}-1.0.0"))
        .status()
        .unwrap();
    assert!(status.success());
    let page = root.join(name);
    std::fs::create_dir_all(&page).unwrap();
    std::fs::write(
        page.join("index.html"),
        format!("<!DOCTYPE html><html><body>\n<a href=\"../files/{archive}\">{archive}</a>\n</body></html>\n"),
    )
    .unwrap();
}

/// Requirements install from wheels only: a package published only as an
/// sdist is refused by name and its build code never runs.
#[cfg(unix)]
#[test]
fn a_requirement_without_a_wheel_is_refused_and_never_built() {
    let home = tempfile::tempdir().unwrap();
    testkit::seed(home.path());
    let index = home.path().join("index");
    let marker = home.path().join("backend-ran");
    sdist_only(&index, "buildsome", &marker);
    let url = testkit::file_url(&index);
    let python = runtime::require(home.path(), testkit::VERSION).unwrap();
    let err = env::lock(
        &uv_with(home.path(), &url),
        &python,
        &["buildsome==1.0.0".to_owned()],
        &home.path().join("lock"),
    )
    .unwrap_err();
    assert!(err.message().contains("buildsome"), "{}", err.message());
    assert!(!marker.exists(), "the sdist's build backend ran");
}

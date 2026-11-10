//! Test support (feature `testkit`): a managed runtime for test homes
//! without network access at test time, and wheel fixtures.
//!
//! uv comes from `PATH` (CI installs the pinned one with
//! `scripts/ci/install-uv`). The interpreter is installed once per
//! checkout by that uv into `target/pybridge-python` (CI warms it before
//! the tests); [`seed`] makes a test home's managed store point at both,
//! exactly where the bridge looks for them.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use pmt_config::schema::PythonVersion;
use sha2::{Digest as _, Sha256};

/// The interpreter version tests run plugins on.
pub const VERSION: PythonVersion = PythonVersion::V3_12;

/// The `uv` on `PATH`.
///
/// # Panics
///
/// When there is none.
#[must_use]
pub fn uv() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(if cfg!(windows) { "uv.exe" } else { "uv" }))
        .find(|candidate| candidate.is_file())
        .expect("the python-bridge tests need uv on PATH (scripts/ci/install-uv installs the pinned one)")
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"),
        PathBuf::from,
    )
}

/// The shared interpreter directory (`target/pybridge-python`), installed
/// on first use.
///
/// # Panics
///
/// When uv cannot install the interpreter.
#[must_use]
pub fn python_root() -> PathBuf {
    let root = target_dir().join("pybridge-python");
    std::fs::create_dir_all(&root).unwrap();
    // Not `<root>/.lock`: uv locks that file itself while installing.
    let guard = std::fs::File::create(root.with_extension("lock")).unwrap();
    pmt_core::file_lock::lock(&guard).unwrap();
    let probe = root.parent().unwrap().join("pybridge-probe");
    let link = probe.join("runtimes").join("python");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    if !link.exists() {
        link_dir(&root, &link);
    }
    if crate::runtime::find(&probe, VERSION).is_none() {
        let status = std::process::Command::new(uv())
            .args([
                "python",
                "install",
                VERSION.as_str(),
                "--no-bin",
                "--install-dir",
            ])
            .arg(&root)
            .env("UV_PYTHON_INSTALL_REGISTRY", "0")
            .status()
            .unwrap();
        assert!(status.success(), "uv could not install CPython {VERSION}");
    }
    root
}

fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

/// Makes `home`'s managed store hold the test interpreter and uv.
///
/// # Panics
///
/// When the store cannot be written.
pub fn seed(home: &Path) {
    let runtimes = home.join("runtimes");
    std::fs::create_dir_all(&runtimes).unwrap();
    let python = runtimes.join("python");
    if !python.exists() {
        link_dir(&python_root(), &python);
    }
    let store = pmt_update::runtimes::Store::open(home);
    let (pin, artifact) = store.pinned("uv").unwrap();
    let dir = runtimes.join("uv").join(pin.dir_name(artifact));
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("uv");
    if !exe.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(uv(), &exe).unwrap();
        #[cfg(windows)]
        std::fs::copy(uv(), &exe).unwrap();
    }
    let record = serde_json::json!({
        "schema": pmt_update::runtimes::RECORD_SCHEMA, "name": "uv", "version": pin.version,
        "sha256": artifact.sha256, "url": artifact.url, "source": "origin",
        "installed_at": "2026-11-01T00:00:00Z", "retries": 0, "resumed": false,
        "exe": "uv", "files": {},
    });
    std::fs::write(
        dir.join(pmt_update::runtimes::RECORD_FILE),
        record.to_string(),
    )
    .unwrap();
}

/// Writes `<dir>/<name>-<version>-py3-none-any.whl` holding `modules`
/// (`(path, source)` pairs) and returns its path.
///
/// # Panics
///
/// When the file cannot be written.
#[must_use]
pub fn wheel(dir: &Path, name: &str, version: &str, modules: &[(&str, &str)]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let dist = format!("{}-{version}", name.replace('-', "_"));
    let path = dir.join(format!("{dist}-py3-none-any.whl"));
    let info = format!("{dist}.dist-info");
    let mut files: Vec<(String, String)> = modules
        .iter()
        .map(|(p, s)| ((*p).to_owned(), (*s).to_owned()))
        .collect();
    files.push((
        format!("{info}/METADATA"),
        format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"),
    ));
    files.push((
        format!("{info}/WHEEL"),
        "Wheel-Version: 1.0\nGenerator: prometheus-testkit\nRoot-Is-Purelib: true\nTag: py3-none-any\n".to_owned(),
    ));
    let mut record = String::new();
    for (file, text) in &files {
        let digest = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(text.as_bytes()));
        let _ = writeln!(record, "{file},sha256={digest},{}", text.len());
    }
    let _ = writeln!(record, "{info}/RECORD,,");
    files.push((format!("{info}/RECORD"), record));
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for (file, text) in files {
        zip.start_file(file, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// A package of a test index: name, version and its modules.
pub type Package<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

/// Writes a PEP 503 simple index of `packages` under `root` (serve it with
/// [`serve`], or use [`file_url`]); no test touches the network.
///
/// # Panics
///
/// When the files cannot be written.
pub fn index(root: &Path, packages: &[Package<'_>]) {
    let files = root.join("files");
    let mut pages: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (name, version, modules) in packages {
        let wheel = wheel(&files, name, version, modules);
        let file = wheel.file_name().unwrap().to_string_lossy().into_owned();
        let digest = crate::hex(&Sha256::digest(std::fs::read(&wheel).unwrap()));
        let project = name.to_ascii_lowercase().replace(['_', '.'], "-");
        let _ = writeln!(
            pages.entry(project).or_default(),
            "<a href=\"../files/{file}#sha256={digest}\">{file}</a>"
        );
    }
    for (project, links) in pages {
        let dir = root.join(&project);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.html"),
            format!("<!DOCTYPE html><html><body>\n{links}</body></html>\n"),
        )
        .unwrap();
    }
}

/// The `file://` URL of an index [`index`] wrote, for `UV_DEFAULT_INDEX`.
#[must_use]
pub fn file_url(root: &Path) -> String {
    format!("file://{}", root.display())
}

/// A loopback HTTP server for a directory written by [`index`]: uv caches
/// what it downloads from it like any registry, so a test can stop it and
/// build offline from the cache. Stops when dropped.
#[derive(Debug)]
pub struct Served {
    /// `http://127.0.0.1:<port>/`.
    pub url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::net::TcpStream::connect(
            self.url.trim_start_matches("http://").trim_end_matches('/'),
        );
    }
}

/// Answers one request for a file under `root`.
fn respond(root: &Path, stream: std::net::TcpStream) {
    use std::io::{BufRead as _, BufReader};
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let path = line
        .split(' ')
        .nth(1)
        .unwrap_or("/")
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .to_owned();
    let mut header = String::new();
    while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
        header.clear();
    }
    let mut file = root.join(path.trim_start_matches('/'));
    if file.is_dir() {
        file = file.join("index.html");
    }
    let reply = match std::fs::read(&file) {
        Ok(body) => {
            let kind = if file.extension().is_some_and(|e| e == "html") {
                "text/html"
            } else {
                "application/octet-stream"
            };
            let mut head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            head.extend(body);
            head
        }
        Err(_) => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let mut stream = stream;
    let _ = stream.write_all(&reply);
}

/// Serves `root` over HTTP on loopback.
///
/// # Panics
///
/// When no port can be bound.
#[must_use]
pub fn serve(root: &Path) -> Served {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (root, flag) = (root.to_path_buf(), std::sync::Arc::clone(&stop));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            respond(&root, stream);
        }
    });
    Served { url, stop }
}

/// A fixture plugin of this crate (`tests/fixtures/plugins/<name>`, else
/// the real Hermes plugins in `tests/fixtures/hermes/<name>`).
#[must_use]
pub fn fixture(name: &str) -> PathBuf {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let plugin = base.join("plugins").join(name);
    if plugin.is_dir() {
        plugin
    } else {
        base.join("hermes").join(name)
    }
}

/// Copies fixture `name` into `<home>/profiles/default/plugins/<id>`.
///
/// # Panics
///
/// When it cannot be copied.
#[must_use]
pub fn place(home: &Path, name: &str, id: &str) -> PathBuf {
    let dir = home.join("profiles/default/plugins").join(id);
    copy_dir(&fixture(name), &dir);
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_name() == "__pycache__" {
            continue;
        }
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

//! `prometheus runtimes`, the doctor's distribution and runtime checks and
//! the browser's use of the store (spec: distribution-and-update, Runtime
//! CLI; Runtime leases and garbage collection; Persistent runtime opt-outs;
//! Lazy runtime installation policy; Doctor reports distribution health).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::daemon::DaemonConfig;
use pmt_update::download::Budget;
use pmt_update::manifest::sha256_hex;
use pmt_update::runtimes::Store;
use pmt_update::runtimes::index::{Format, Pin, PinnedArtifact, RuntimeIndex};
use pmt_update::runtimes::install::{Installer, Sources};
use pmt_update::runtimes::lease::Lease;
use prometheus::runtimes_cmd::browser;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn code(out: &std::process::Output) -> i32 {
    out.status.code().unwrap()
}

#[test]
fn an_unknown_runtime_exits_2_naming_the_expected_ones() {
    let home = common::temp_home();
    let out = common::run(home.path(), &["runtimes", "install", "java"]);
    assert_eq!(code(&out), 2);
    assert!(
        common::stderr(&out)
            .contains("unknown runtime 'java' (expected node|cpython|uv|chromium|doc-converter)"),
        "{}",
        common::stderr(&out)
    );
}

#[test]
fn exclusions_are_edited_persisted_and_refuse_installs() {
    let home = common::temp_home();
    let h = home.path();
    common::ok(&common::run(h, &["runtimes", "exclude", "add", "chromium"]));
    assert_eq!(
        DaemonConfig::load(h).unwrap().runtimes.exclude,
        ["chromium"]
    );
    assert_eq!(
        common::ok(&common::run(h, &["runtimes", "exclude", "list"])),
        "chromium\n"
    );
    let out = common::run(h, &["runtimes", "install", "chromium"]);
    assert_eq!(code(&out), 1);
    assert!(
        common::stderr(&out).contains(
            "chromium is excluded by runtimes.exclude: run 'prometheus runtimes exclude remove chromium' to allow it"
        ),
        "{}",
        common::stderr(&out)
    );
    assert!(
        !h.join("runtimes/.downloads").exists(),
        "nothing downloaded"
    );
    let list = common::ok(&common::run(h, &["runtimes", "list"]));
    assert!(list.contains("chromium 156"), "{list}");
    assert!(list.contains("(excluded)"), "{list}");
    let out = common::run(h, &["runtimes", "exclude", "add", "java"]);
    assert_eq!(code(&out), 2);
    common::ok(&common::run(
        h,
        &["runtimes", "exclude", "remove", "chromium"],
    ));
    assert!(DaemonConfig::load(h).unwrap().runtimes.exclude.is_empty());
}

#[test]
fn a_corrupted_runtime_is_reported_by_verify_and_doctor() {
    let home = common::temp_home();
    let h = home.path();
    let dir = common::fake_runtime(h, "node", "1.0.0-aaaaaaaaaaaa", &[("bin/node", b"node")]);
    let out = common::run(h, &["runtimes", "verify"]);
    assert_eq!(code(&out), 0, "{}", common::stdout(&out));
    std::fs::write(dir.join("bin/node"), b"tampered").unwrap();
    let out = common::run(h, &["runtimes", "verify"]);
    assert_eq!(code(&out), 1);
    assert!(
        common::stdout(&out).contains("✗ node: realized bytes do not match recorded digest"),
        "{}",
        common::stdout(&out)
    );
    let out = common::run(h, &["doctor", "runtimes"]);
    assert_eq!(code(&out), 1);
    assert!(common::stdout(&out).contains("node: realized bytes do not match recorded digest"));
}

#[test]
fn in_use_runtime_survives_gc() {
    let home = common::temp_home();
    let h = home.path();
    let old = common::fake_runtime(
        h,
        "cpython",
        "3.11.0-cccccccccccc",
        &[("bin/python3", b"py")],
    );
    let lease = Lease::acquire(&h.join("runtimes"), "cpython", "3.11.0-cccccccccccc").unwrap();
    let out = common::ok(&common::run(h, &["runtimes", "gc"]));
    assert!(
        out.contains(&format!("kept {} (in use)", old.display())),
        "{out}"
    );
    assert!(old.is_dir());
    let list = common::ok(&common::run(h, &["runtimes", "list"]));
    assert!(list.contains("3.11.0-cccccccccccc unpinned"), "{list}");
    assert!(list.contains("1 user(s)"), "{list}");
    drop(lease);
    let out = common::ok(&common::run(h, &["runtimes", "gc"]));
    assert!(out.contains(&format!("removed {}", old.display())), "{out}");
    let reclaimed: u64 = out
        .lines()
        .find_map(|l| l.strip_prefix("reclaimed "))
        .and_then(|l| l.trim_end_matches(" bytes").parse().ok())
        .unwrap();
    assert!(reclaimed >= 2, "{out}");
    assert!(!old.exists());
}

#[test]
fn doctor_surfaces_an_interrupted_update() {
    let home = common::temp_home();
    let h = home.path();
    let receipts = h.join("updates/receipts");
    std::fs::create_dir_all(&receipts).unwrap();
    let receipt = serde_json::json!({
        "id": "01J", "kind": "update", "outcome": "interrupted",
        "started_at": "2026-10-31T00:00:00Z", "finished_at": null,
        "pid": 1, "pid_started": 1, "binary": "/opt/prometheus", "from": "v1.4.0",
        "to": "v1.4.1", "channel": "stable",
        "stages": ["check", "download", "verify", "unpack", "self-test", "backup"],
        "follow_ups": [], "errors": [], "snapshots": [],
    });
    std::fs::write(
        receipts.join("20261031T000000Z-01J.json"),
        receipt.to_string(),
    )
    .unwrap();
    let out = common::run(h, &["doctor", "distribution"]);
    assert_eq!(code(&out), 1);
    let text = common::stdout(&out);
    assert!(
        text.contains("last update interrupted at stage swap"),
        "{text}"
    );
    assert!(
        text.contains("hint: run 'prometheus update' again"),
        "{text}"
    );
    assert!(text.contains("owner dev, channel stable"), "{text}");
    assert!(text.contains("release keys: primary "), "{text}");
}

#[test]
fn version_verbose_prints_the_runtime_index_digest() {
    let home = common::temp_home();
    let out = common::ok(&common::run(home.path(), &["--version", "--verbose"]));
    let digest = RuntimeIndex::embedded_digest();
    assert!(
        out.contains(&format!("runtime_index_sha256: {digest}")),
        "{out}"
    );
    let plain = common::ok(&common::run(home.path(), &["--version"]));
    assert_eq!(plain.lines().count(), 1, "{plain}");
}

/// The browser's lazy install through the store, against a local origin.
struct Origin {
    _dir: tempfile::TempDir,
    root: PathBuf,
    server: MockServer,
    runtime: tokio::runtime::Runtime,
}

impl Origin {
    fn new() -> Self {
        let dir = common::temp_home();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "chrome/chrome", &b"exe!"[..])
            .unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        runtime.block_on(
            Mock::given(method("GET"))
                .and(path("/chrome.tar.gz"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .mount(&server),
        );
        std::fs::write(dir.path().join("archive"), &bytes).unwrap();
        Self {
            root: dir.path().join("runtimes"),
            _dir: dir,
            server,
            runtime,
        }
    }

    fn installer(&self, exclude: &[&str]) -> Installer {
        let bytes = std::fs::read(self.root.with_file_name("archive")).unwrap();
        let index = RuntimeIndex {
            schema: pmt_update::runtimes::index::INDEX_SCHEMA.to_owned(),
            runtimes: BTreeMap::from([(
                "chromium".to_owned(),
                Pin {
                    version: "1.0".to_owned(),
                    artifacts: BTreeMap::from([(
                        "test".to_owned(),
                        PinnedArtifact {
                            url: format!("{}/chrome.tar.gz", self.server.uri()),
                            size: bytes.len() as u64,
                            sha256: sha256_hex(&bytes),
                            format: Format::TarGz,
                            exe: "chrome/chrome".to_owned(),
                        },
                    )]),
                },
            )]),
        };
        let store = Store::new(self.root.clone(), index, "test".to_owned());
        let sources = Sources {
            exclude: exclude.iter().map(|s| (*s).to_owned()).collect(),
            ..Sources::default()
        };
        Installer::new(
            store,
            &pmt_net::OutboundPolicy::default(),
            sources,
            Arc::new(|_| {}),
        )
        .with_budget(Budget::default())
    }

    fn requests(&self) -> usize {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .len()
    }
}

fn daemon_config(allow: bool, exclude: &[&str]) -> DaemonConfig {
    let mut config = DaemonConfig::default();
    config.runtimes.allow_lazy_install = allow;
    config.runtimes.exclude = exclude.iter().map(|s| (*s).to_owned()).collect();
    config
}

fn no_host(mut policy: pmt_tools::browser::RuntimePolicy) -> pmt_tools::browser::RuntimePolicy {
    policy.host_candidates.clear();
    policy
}

#[test]
fn lazy_install_disabled_fails_the_browser_and_downloads_nothing() {
    let origin = Origin::new();
    let policy = no_host(browser::policy(
        origin.installer(&[]),
        &daemon_config(false, &[]),
    ));
    assert_eq!(
        policy.availability("chromium"),
        Err("lazy installs disabled")
    );
    let err = origin
        .runtime
        .block_on(policy.executable("chromium"))
        .unwrap_err();
    assert_eq!(
        err,
        "chromium is not installed and lazy installs are disabled: run 'prometheus runtimes install chromium'"
    );
    assert_eq!(origin.requests(), 0);
}

#[test]
fn an_excluded_browser_runtime_is_never_lazily_installed() {
    let origin = Origin::new();
    let excluded = ["chromium"];
    let policy = no_host(browser::policy(
        origin.installer(&excluded),
        &daemon_config(true, &excluded),
    ));
    assert_eq!(policy.availability("chromium"), Err("excluded"));
    let err = origin
        .runtime
        .block_on(policy.executable("chromium"))
        .unwrap_err();
    assert_eq!(
        err,
        "chromium is excluded by runtimes.exclude: run 'prometheus runtimes exclude remove chromium' to allow it"
    );
    assert_eq!(origin.requests(), 0);
}

#[test]
fn a_missing_browser_runtime_is_installed_on_first_use() {
    let origin = Origin::new();
    let policy = no_host(browser::policy(
        origin.installer(&[]),
        &daemon_config(true, &[]),
    ));
    assert_eq!(policy.availability("chromium"), Ok(()));
    let exe = origin
        .runtime
        .block_on(policy.executable("chromium"))
        .unwrap();
    assert!(exe.ends_with("chrome/chrome"), "{}", exe.display());
    assert_eq!(std::fs::read(&exe).unwrap(), b"exe!");
    assert_eq!(origin.requests(), 1);
    // Installed now: no second download.
    origin
        .runtime
        .block_on(policy.executable("chromium"))
        .unwrap();
    assert_eq!(origin.requests(), 1);
}

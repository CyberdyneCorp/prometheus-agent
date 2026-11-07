//! The managed runtime store against a local origin and mirror (spec:
//! distribution-and-update, Managed runtime store; Download retry budget;
//! Runtime leases and garbage collection; Lazy runtime installation
//! policy; Persistent runtime opt-outs; Download mirrors).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pmt_update::download::Budget;
use pmt_update::manifest::sha256_hex;
use pmt_update::runtimes::index::{Format, Pin, PinnedArtifact, RuntimeIndex};
use pmt_update::runtimes::install::{Installer, Sources};
use pmt_update::runtimes::lazy::{Lazy, Unavailable};
use pmt_update::runtimes::lease::Lease;
use pmt_update::runtimes::{RECORD_FILE, Store, gc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const PLATFORM: &str = "test-platform";

/// `node-v1/bin/node` (+ a README) as a `.tar.gz`.
fn archive() -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for (name, data, mode) in [
        ("node-v1/bin/node", &b"#!/bin/sh\necho node\n"[..], 0o755),
        ("node-v1/README.md", &[b'x'; 4096][..], 0o644),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        tar.append_data(&mut header, name, data).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

/// An index pinning `node` 1.0.0 at `<origin>/dist/node-v1.tar.gz`.
fn index(origin: &str, bytes: &[u8]) -> RuntimeIndex {
    let artifact = PinnedArtifact {
        url: format!("{origin}/dist/node-v1.tar.gz"),
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
        format: Format::TarGz,
        exe: "node-v1/bin/node".to_owned(),
    };
    RuntimeIndex {
        schema: pmt_update::runtimes::index::INDEX_SCHEMA.to_owned(),
        runtimes: BTreeMap::from([(
            "node".to_owned(),
            Pin {
                version: "1.0.0".to_owned(),
                artifacts: BTreeMap::from([(PLATFORM.to_owned(), artifact)]),
            },
        )]),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    server: MockServer,
    bytes: Vec<u8>,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dir.path().join("runtimes");
        Self {
            _dir: dir,
            root,
            server: MockServer::start().await,
            bytes: archive(),
        }
    }

    fn store(&self) -> Store {
        Store::new(
            self.root.clone(),
            index(&self.server.uri(), &self.bytes),
            PLATFORM.to_owned(),
        )
    }

    fn installer(&self, sources: Sources) -> Installer {
        Installer::new(
            self.store(),
            &pmt_net::OutboundPolicy::default(),
            sources,
            Arc::new(|_| {}),
        )
        .with_budget(Budget {
            max_stalled: 4,
            base: Duration::from_millis(5),
            cap: Duration::from_millis(20),
        })
    }

    async fn serve(&self, at: &str, body: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(Ranged(body))
            .mount(&self.server)
            .await;
    }

    async fn requests(&self, prefix: &str) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().starts_with(prefix))
            .count()
    }

    fn downloads_left(&self) -> usize {
        std::fs::read_dir(self.root.join(".downloads")).map_or(0, |d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "part"))
                .count()
        })
    }

    fn versions(&self) -> Vec<String> {
        std::fs::read_dir(self.root.join("node")).map_or(Vec::new(), |d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
    }
}

/// Serves a body, honouring `Range: bytes=N-` with 206 like a CDN.
struct Ranged(Vec<u8>);

impl Respond for Ranged {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let start = request
            .headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
        match start {
            Some(start) if start < self.0.len() => ResponseTemplate::new(206)
                .insert_header(
                    "content-range",
                    format!("bytes {start}-{}/{}", self.0.len() - 1, self.0.len()),
                )
                .set_body_bytes(self.0[start..].to_vec()),
            Some(_) => ResponseTemplate::new(416),
            None => ResponseTemplate::new(200).set_body_bytes(self.0.clone()),
        }
    }
}

#[tokio::test]
async fn a_digest_mismatch_deletes_the_download_and_installs_nothing() {
    let f = Fixture::new().await;
    let mut tampered = f.bytes.clone();
    tampered[100] ^= 0xff;
    f.serve("/dist/node-v1.tar.gz", tampered.clone()).await;
    let err = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "sha256 mismatch for {}/dist/node-v1.tar.gz: pinned {}, got {}",
            f.server.uri(),
            sha256_hex(&f.bytes),
            sha256_hex(&tampered)
        )
    );
    assert_eq!(err.exit_code(), 77);
    assert_eq!(f.downloads_left(), 0, "the download is deleted");
    assert!(f.versions().is_empty(), "nothing is installed");
    assert!(f.store().installed("node").is_none());
}

/// An installer builds its HTTP client only when it downloads: every
/// session start makes installers (browser, document tools) that rarely
/// install anything, and a client build loads the system's root
/// certificates. An installer whose client cannot be built (a bad
/// `network.proxy`) is therefore made without error, serves what is
/// installed, and fails only when it has to download.
#[tokio::test]
async fn an_installer_configures_its_http_client_only_when_it_downloads() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let unbuildable = |store: Store| {
        Installer::new(
            store,
            &pmt_net::OutboundPolicy {
                proxy: Some("not a proxy url".to_owned()),
                ..pmt_net::OutboundPolicy::default()
            },
            Sources::default(),
            Arc::new(|_| {}),
        )
    };
    let err = unbuildable(f.store()).install("node").await.unwrap_err();
    assert_eq!(err.code().code, "net.client_init", "{}", err.message());

    f.installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    let installed = unbuildable(f.store()).install("node").await.unwrap();
    assert!(
        installed.dir.starts_with(&f.root),
        "{}",
        installed.dir.display()
    );
}

#[tokio::test]
async fn an_install_is_verified_unpacked_and_renamed_into_place() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let installed = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    let sha12 = &sha256_hex(&f.bytes)[..12];
    assert_eq!(
        installed.dir,
        f.root.join("node").join(format!("1.0.0-{sha12}"))
    );
    assert!(installed.exe().is_file());
    assert_eq!(installed.record.source, "origin");
    assert_eq!(installed.record.retries, 0);
    assert!(installed.dir.join(RECORD_FILE).is_file());
    assert_eq!(f.downloads_left(), 0);
    assert!(f.store().verify().iter().all(|v| v.mismatch.is_none()));
    // A second install is a no-op.
    f.installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    assert_eq!(f.requests("/dist/").await, 1);
}

#[tokio::test]
async fn an_interrupted_download_resumes_and_is_verified() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    // A download killed at 60% left its partial file, and a staging
    // directory that never got renamed.
    let downloads = f.root.join(".downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let sha12 = &sha256_hex(&f.bytes)[..12];
    let partial = downloads.join(format!("node-1.0.0-{sha12}-node-v1.tar.gz.part"));
    std::fs::write(&partial, &f.bytes[..f.bytes.len() * 6 / 10]).unwrap();
    std::fs::create_dir_all(f.root.join(".staging/01HALF/node-v1")).unwrap();
    assert!(
        f.store().installed("node").is_none(),
        "no partial runtime is used"
    );
    let installed = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    assert!(
        installed.record.resumed,
        "the download resumed with a range request"
    );
    let range = f.server.received_requests().await.unwrap()[0]
        .headers
        .get("range")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(range, Some(format!("bytes={}-", f.bytes.len() * 6 / 10)));
    assert!(f.store().verify().iter().all(|v| v.mismatch.is_none()));
}

#[tokio::test]
async fn a_flaky_origin_is_retried_and_the_record_counts_the_retries() {
    let f = Fixture::new().await;
    Mock::given(method("GET"))
        .and(path("/dist/node-v1.tar.gz"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&f.server)
        .await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let installed = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    assert_eq!(installed.record.retries, 2);
    assert_eq!(f.requests("/dist/").await, 3);
}

#[tokio::test]
async fn the_budget_gives_up_after_four_failures_without_progress() {
    let f = Fixture::new().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&f.server)
        .await;
    let err = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap_err();
    assert!(err.message().contains("4 consecutive failures"), "{err}");
    assert_eq!(err.exit_code(), 69);
    assert_eq!(f.requests("/dist/").await, 4);
}

#[tokio::test]
async fn a_permanent_failure_is_not_retried() {
    let f = Fixture::new().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&f.server)
        .await;
    let err = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap_err();
    assert!(err.message().contains("404"), "{err}");
    assert_eq!(f.requests("/dist/").await, 1);
}

#[tokio::test]
async fn a_mirror_serving_a_tampered_archive_installs_nothing() {
    let f = Fixture::new().await;
    let mut tampered = f.bytes.clone();
    tampered[10] ^= 1;
    f.serve("/mirror/node/1.0.0/node-v1.tar.gz", tampered).await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let sources = Sources {
        mirror: Some(format!("{}/mirror/", f.server.uri())),
        ..Sources::default()
    };
    let err = f.installer(sources).install("node").await.unwrap_err();
    assert!(
        err.message().starts_with(&format!(
            "sha256 mismatch for {}/mirror/node/1.0.0/node-v1.tar.gz: pinned {}",
            f.server.uri(),
            sha256_hex(&f.bytes)
        )),
        "{err}"
    );
    assert!(f.versions().is_empty());
    assert_eq!(f.requests("/dist/").await, 0, "the origin is not asked");
}

#[tokio::test]
async fn a_failing_mirror_is_named_unless_fallback_is_on() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let mirror = format!("{}/mirror", f.server.uri());
    let err = f
        .installer(Sources {
            mirror: Some(mirror.clone()),
            ..Sources::default()
        })
        .install("node")
        .await
        .unwrap_err();
    assert!(
        err.message().starts_with(&format!(
            "cannot download {mirror}/node/1.0.0/node-v1.tar.gz"
        )),
        "{err}"
    );
    let installed = f
        .installer(Sources {
            mirror: Some(mirror),
            fallback: true,
            ..Sources::default()
        })
        .install("node")
        .await
        .unwrap();
    assert_eq!(installed.record.source, "origin");
}

#[tokio::test]
async fn an_excluded_runtime_is_never_downloaded() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let sources = Sources {
        exclude: vec!["node".to_owned()],
        ..Sources::default()
    };
    let err = f
        .installer(sources.clone())
        .install("node")
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "node is excluded by runtimes.exclude: run 'prometheus runtimes exclude remove node' to allow it"
    );
    assert_eq!(err.exit_code(), 1);
    let lazy = Lazy::new(f.installer(sources), true, None);
    assert_eq!(lazy.availability("node"), Err(Unavailable::Excluded));
    assert_eq!(Unavailable::Excluded.reason(), "excluded");
    let err = lazy.executable("node").await.unwrap_err();
    assert!(
        err.message().contains("excluded by runtimes.exclude"),
        "{err}"
    );
    assert_eq!(f.requests("/").await, 0);
}

#[tokio::test]
async fn lazy_installs_follow_the_policy() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let disabled = Lazy::new(f.installer(Sources::default()), false, None);
    assert_eq!(
        disabled.availability("node"),
        Err(Unavailable::LazyInstallsDisabled)
    );
    let err = disabled.executable("node").await.unwrap_err();
    assert_eq!(
        err.message(),
        "node is not installed and lazy installs are disabled: run 'prometheus runtimes install node'"
    );
    let declined = Lazy::new(
        f.installer(Sources::default()),
        true,
        Some(Box::new(|q: String| {
            assert!(q.starts_with("Install node 1.0.0 ("), "{q}");
            assert!(q.ends_with(")? [Y/n]"), "{q}");
            Box::pin(async { false })
        })),
    );
    assert_eq!(
        declined.executable("node").await.unwrap_err().exit_code(),
        130
    );
    assert_eq!(f.requests("/").await, 0, "nothing downloaded so far");
    let lazy = Lazy::new(f.installer(Sources::default()), true, None);
    assert_eq!(lazy.availability("node"), Ok(()));
    let exe = lazy.executable("node").await.unwrap();
    assert!(exe.ends_with("node-v1/bin/node"));
    assert_eq!(
        pmt_update::runtimes::lease::users(&f.root, "node", &dir_of(&exe, &f.root)),
        1
    );
    drop(lazy);
    assert_eq!(
        pmt_update::runtimes::lease::users(&f.root, "node", &dir_of(&exe, &f.root)),
        0
    );
}

fn dir_of(exe: &Path, root: &Path) -> String {
    exe.strip_prefix(root.join("node"))
        .unwrap()
        .components()
        .next()
        .unwrap()
        .as_os_str()
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn in_use_runtime_survives_gc_until_its_lease_is_released() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let pinned = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    // An older, unpinned version a bridge process still runs on.
    let old = f.root.join("node/0.9.0-aaaaaaaaaaaa");
    std::fs::create_dir_all(old.join("bin")).unwrap();
    std::fs::write(old.join("bin/node"), b"old").unwrap();
    let stale = f.root.join("node/0.8.0-bbbbbbbbbbbb");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("x"), b"xyz").unwrap();
    let lease = Lease::acquire(&f.root, "node", "0.9.0-aaaaaaaaaaaa").unwrap();
    let store = f.store();
    let pins: BTreeMap<String, BTreeSet<String>> = store
        .pins()
        .into_iter()
        .map(|(n, d)| (n, BTreeSet::from([d])))
        .collect();
    let report = gc::collect(&store, &pins, SystemTime::now()).unwrap();
    assert_eq!(report.removed, [stale]);
    assert_eq!(report.leased, std::slice::from_ref(&old));
    assert_eq!(report.reclaimed, 3);
    assert!(pinned.dir.is_dir(), "the pinned version stays");
    drop(lease);
    let report = gc::collect(&store, &pins, SystemTime::now()).unwrap();
    assert_eq!(report.removed, [old]);
    assert!(pinned.dir.is_dir());
}

#[tokio::test]
async fn gc_removes_partial_downloads_older_than_a_day() {
    let f = Fixture::new().await;
    let downloads = f.root.join(".downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("node-x.part"), b"12345").unwrap();
    let store = f.store();
    let now = SystemTime::now();
    let report = gc::collect(&store, &BTreeMap::new(), now).unwrap();
    assert!(report.removed.is_empty(), "a fresh partial download stays");
    let later = now + Duration::from_hours(25);
    let report = gc::collect(&store, &BTreeMap::new(), later).unwrap();
    assert_eq!(report.removed, [downloads.join("node-x.part")]);
    assert_eq!(report.reclaimed, 5);
}

#[tokio::test]
async fn verify_reports_a_modified_file() {
    let f = Fixture::new().await;
    f.serve("/dist/node-v1.tar.gz", f.bytes.clone()).await;
    let installed = f
        .installer(Sources::default())
        .install("node")
        .await
        .unwrap();
    std::fs::write(installed.dir.join("node-v1/README.md"), b"changed").unwrap();
    let verified = f.store().verify();
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].mismatch.as_deref(), Some("node-v1/README.md"));
}

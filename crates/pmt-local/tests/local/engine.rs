//! Hash-pinned engine installation (spec: local-models, Hash-pinned engine
//! installation).

use std::collections::BTreeMap;

use pmt_config::daemon_local::LocalBackend;
use pmt_local::download::{Control, Downloader, Jobs};
use pmt_local::engine::{Build, Engine, EngineFile, EngineIndex, Outcome};
use pmt_update::download::Budget;
use pmt_update::runtimes::index::Format;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{client, sha, tar_gz, temp};

fn index(url: &str, archive: &[u8], pinned_sha: &str) -> EngineIndex {
    let build = Build {
        files: vec![EngineFile {
            url: url.to_owned(),
            size: archive.len() as u64,
            sha256: pinned_sha.to_owned(),
            format: Format::TarGz,
        }],
        exe: "llama-b1/llama-server".to_owned(),
        library_dirs: vec!["llama-b1".to_owned()],
    };
    EngineIndex {
        schema: pmt_local::engine::index::SCHEMA.to_owned(),
        version: "b1".to_owned(),
        builds: BTreeMap::from([("linux-x64/vulkan".to_owned(), build)]),
    }
}

fn downloader() -> Downloader {
    Downloader::new(client()).with_budget(Budget {
        max_stalled: 1,
        base: std::time::Duration::from_millis(1),
        cap: std::time::Duration::from_millis(1),
    })
}

#[tokio::test]
async fn a_corrupted_archive_is_deleted_nothing_is_installed_and_it_exits_74() {
    let server = MockServer::start().await;
    let archive = tar_gz(&[("llama-b1/llama-server", b"#!/bin/sh\n")]);
    let mut corrupted = archive.clone();
    corrupted[archive.len() / 2] ^= 0xff;
    Mock::given(method("GET"))
        .and(path("/llama-b1-bin-ubuntu-vulkan-x64.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(corrupted))
        .mount(&server)
        .await;
    let url = format!("{}/llama-b1-bin-ubuntu-vulkan-x64.tar.gz", server.uri());
    let home = temp();
    let root = home.path().join("runtimes/llamacpp");
    let engine = Engine::new(root.clone(), index(&url, &archive, &sha(&archive)));
    let jobs = Jobs::open(home.path());
    let err = engine
        .install(
            "linux-x64",
            LocalBackend::Vulkan,
            &downloader(),
            &jobs,
            &Control::default(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 74);
    assert_eq!(err.code().code, "local.digest_mismatch");
    assert!(
        err.message()
            .contains("llama-b1-bin-ubuntu-vulkan-x64.tar.gz"),
        "{}",
        err.message()
    );
    assert!(engine.installed(LocalBackend::Vulkan).is_none());
    assert!(engine.installed_all().is_empty());
    let leftovers: Vec<_> = walk(home.path())
        .into_iter()
        .filter(|p| p.extension().is_some_and(|x| x == "part" || x == "gz"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the archive was deleted: {leftovers:?}"
    );
    assert!(jobs.all().is_empty(), "no job is left to resume");
}

#[tokio::test]
async fn a_verified_archive_is_unpacked_into_version_backend() {
    let server = MockServer::start().await;
    let archive = tar_gz(&[("llama-b1/llama-server", b"#!/bin/sh\n")]);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive.clone()))
        .mount(&server)
        .await;
    let url = format!("{}/llama-b1-bin-ubuntu-vulkan-x64.tar.gz", server.uri());
    let home = temp();
    let engine = Engine::new(
        home.path().join("runtimes/llamacpp"),
        index(&url, &archive, &sha(&archive)),
    );
    let jobs = Jobs::open(home.path());
    let outcome = engine
        .install(
            "linux-x64",
            LocalBackend::Vulkan,
            &downloader(),
            &jobs,
            &Control::default(),
            &mut |_| {},
        )
        .await
        .unwrap();
    let Outcome::Installed(installed) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(installed.dir.ends_with("runtimes/llamacpp/b1-vulkan"));
    assert!(installed.exe().is_file());
    assert_eq!(installed.library_dirs(), [installed.dir.join("llama-b1")]);
    assert_eq!(engine.installed(LocalBackend::Vulkan), Some(installed));
    assert!(jobs.all().is_empty());
}

#[tokio::test]
async fn hip_on_macos_is_refused_with_69_before_any_request() {
    let server = MockServer::start().await;
    let home = temp();
    let engine = Engine::new(
        home.path().join("runtimes/llamacpp"),
        EngineIndex::embedded(),
    );
    let err = engine
        .install(
            "darwin-arm64",
            LocalBackend::Hip,
            &downloader(),
            &Jobs::open(home.path()),
            &Control::default(),
            &mut |_| {},
        )
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 69);
    assert!(
        err.message().contains("(hip, darwin-arm64)"),
        "{}",
        err.message()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        !home.path().join("cache").exists(),
        "no download was planned"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

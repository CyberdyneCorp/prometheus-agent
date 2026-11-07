//! Post-setup against a runtime store with a test index served by a local
//! origin (spec: tool-system, Tool configuration summary and post-setup —
//! Browser needs a runtime).

use std::collections::BTreeMap;
use std::sync::Arc;

use pmt_config::Config;
use pmt_config::daemon::DaemonConfig;
use pmt_tools::{Builtins, ToolRegistry};
use pmt_update::manifest::sha256_hex;
use pmt_update::runtimes::Store;
use pmt_update::runtimes::index::{Format, INDEX_SCHEMA, Pin, PinnedArtifact, RuntimeIndex};
use pmt_update::runtimes::install::{Installer, Sources};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{Facts, Step, install, pending, runtimes};

const PLATFORM: &str = "test-platform";

/// `<name>/bin/<name>` as a `.tar.gz`.
fn archive(name: &str) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let data = b"#!/bin/sh\nexit 0\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, format!("{name}/bin/{name}"), &data[..])
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

fn pin(origin: &str, name: &str, bytes: &[u8]) -> Pin {
    Pin {
        version: "1.0.0".to_owned(),
        artifacts: BTreeMap::from([(
            PLATFORM.to_owned(),
            PinnedArtifact {
                url: format!("{origin}/dist/{name}.tar.gz"),
                size: bytes.len() as u64,
                sha256: sha256_hex(bytes),
                format: Format::TarGz,
                exe: format!("{name}/bin/{name}"),
            },
        )]),
    }
}

fn registry() -> ToolRegistry {
    let builtins = Builtins::standard(crate::tools_cmd::builtin_tools(std::path::Path::new("")));
    ToolRegistry::new("test", Arc::new(builtins.unwrap()))
}

fn no_secrets(_: &str) -> bool {
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn the_browser_runtime_is_installed_and_the_tools_become_available() {
    let dir = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
    let server = MockServer::start().await;
    let mut pins = BTreeMap::new();
    for name in ["chromium", "cpython"] {
        let bytes = archive(name);
        pins.insert(name.to_owned(), pin(&server.uri(), name, &bytes));
        Mock::given(method("GET"))
            .and(path(format!("/dist/{name}.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
    }
    let index = RuntimeIndex {
        schema: INDEX_SCHEMA.to_owned(),
        runtimes: pins,
    };
    let store = Store::new(dir.path().join("runtimes"), index, PLATFORM.to_owned());
    let config = Config::default();
    let mut daemon = DaemonConfig::default();
    daemon.runtimes.allow_lazy_install = false;
    let facts = Facts {
        config: &config,
        store: &store,
        host_browsers: Vec::new(),
        host_python: false,
        has_secret: &no_secrets,
    };
    let registry = registry();

    let steps = pending(&registry, "browser", &facts).unwrap();
    assert_eq!(
        steps,
        vec![Step::Runtime("chromium"), Step::Runtime("cpython")]
    );
    assert!(
        steps[0]
            .describe(&store)
            .starts_with("install chromium 1.0.0 (")
    );
    let host_free = pmt_tools::browser::launch::system_candidates(None).is_empty();
    let before =
        crate::runtimes_cmd::browser::availability(store.clone(), &daemon, &config.browser, None);
    if host_free {
        assert_eq!(before, Err("lazy installs disabled"));
    }

    let installer = Installer::new(
        store.clone(),
        &pmt_net::OutboundPolicy::default(),
        Sources::default(),
        Arc::new(|_| {}),
    );
    let done = install(&installer, &runtimes(&steps)).await.unwrap();
    assert_eq!(done.len(), 2, "{done:?}");

    assert!(pending(&registry, "browser", &facts).unwrap().is_empty());
    assert_eq!(
        crate::runtimes_cmd::browser::availability(store, &daemon, &config.browser, None),
        Ok(())
    );
}

#[tokio::test]
async fn an_excluded_runtime_is_refused_before_any_download() {
    let dir = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
    let server = MockServer::start().await;
    let bytes = archive("chromium");
    let index = RuntimeIndex {
        schema: INDEX_SCHEMA.to_owned(),
        runtimes: BTreeMap::from([(
            "chromium".to_owned(),
            pin(&server.uri(), "chromium", &bytes),
        )]),
    };
    let store = Store::new(dir.path().join("runtimes"), index, PLATFORM.to_owned());
    let installer = Installer::new(
        store,
        &pmt_net::OutboundPolicy::default(),
        Sources {
            exclude: vec!["chromium".to_owned()],
            ..Sources::default()
        },
        Arc::new(|_| {}),
    );
    let err = install(&installer, &["chromium"]).await.unwrap_err();
    assert_eq!(err.code().code, "runtime.excluded");
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

/// Composite toolsets carry the steps of the toolsets they include; a
/// toolset without browser tools has none of the browser's.
#[test]
fn composites_carry_the_steps_of_their_members() {
    let dir = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
    let store = Store::new(
        dir.path().join("runtimes"),
        RuntimeIndex::embedded(),
        PLATFORM.to_owned(),
    );
    let config = Config::default();
    let facts = Facts {
        config: &config,
        store: &store,
        host_browsers: Vec::new(),
        host_python: true,
        has_secret: &no_secrets,
    };
    let registry = registry();
    let core = pending(&registry, "core", &facts).unwrap();
    assert!(core.contains(&Step::Runtime("chromium")), "{core:?}");
    assert!(
        core.iter()
            .any(|s| matches!(s, Step::Configure { what, .. } if what.starts_with("web search"))),
        "{core:?}"
    );
    let file = pending(&registry, "file", &facts).unwrap();
    assert!(file.is_empty(), "{file:?}");
    let telegram = pending(&registry, "platform-telegram", &facts).unwrap();
    assert_eq!(core, telegram);
}

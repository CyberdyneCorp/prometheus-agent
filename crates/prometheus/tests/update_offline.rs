//! `prometheus update --dry-run` and offline bundles against a local
//! release server (spec: distribution-and-update, Update dry-run and
//! changelog preview; Offline bundles).

mod common;

use std::path::{Path, PathBuf};

use common::release::{Fixture, code, next_patch};
use pmt_update::Channel;
use pmt_update::ReleaseVersion;
use pmt_update::manifest::{ReleaseDetails, ReleaseNotes, Schemas};

/// Every file under `dir` with its bytes, to prove nothing changed.
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files: Vec<(PathBuf, Vec<u8>)> = common::files_under(dir)
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    files.sort();
    files
}

fn notes(version: &str, breaking: &[&str], fixes: &[&str]) -> ReleaseNotes {
    ReleaseNotes {
        version: ReleaseVersion::parse(version).unwrap(),
        breaking: breaking.iter().map(|s| (*s).to_owned()).collect(),
        features: Vec::new(),
        fixes: fixes.iter().map(|s| (*s).to_owned()).collect(),
        security: Vec::new(),
    }
}

#[test]
fn preview_a_breaking_update() {
    let f = Fixture::self_install();
    common::ok(&f.run(&["config", "set", "model.default", "mock"]));
    let next = next_patch(f.version);
    f.publish_with(
        &next,
        Channel::Stable,
        &f.bytes(),
        None,
        ReleaseDetails {
            notes: vec![
                notes(&next, &["config v1→v2: [display] keys moved"], &["a fix"]),
                notes(&f.version.to_string(), &[], &["installed already"]),
            ],
            schemas: Some(Schemas {
                config: pmt_config::schema::SCHEMA_VERSION + 1,
                store: pmt_store::SCHEMA_VERSION,
            }),
            ..ReleaseDetails::default()
        },
    );
    let profiles = tree(&f.home.join("profiles"));
    let before = f.bytes();
    let out = f.run(&["update", "--dry-run"]);
    assert_eq!(code(&out), 10, "{}", common::stderr(&out));
    let text = common::stdout(&out);
    assert!(
        text.contains(&format!(
            "update available: {} → {next} (stable)",
            f.version
        )),
        "{text}"
    );
    assert!(
        text.contains("Breaking:\n  - config v1→v2: [display] keys moved"),
        "{text}"
    );
    assert!(!text.contains("installed already"), "{text}");
    let config = pmt_config::schema::SCHEMA_VERSION;
    assert!(
        text.contains(&format!("  default: config v{config}→v{}", config + 1)),
        "{text}"
    );
    assert!(text.contains("nothing was downloaded or changed"), "{text}");
    assert_eq!(f.bytes(), before, "binary unchanged");
    assert_eq!(
        tree(&f.home.join("profiles")),
        profiles,
        "profiles unchanged"
    );
    let archive = pmt_update::manifest::Artifact::file_name(&f.target);
    assert!(
        f.requests()
            .iter()
            .all(|r| !r.url.path().ends_with(&archive)),
        "the archive is not downloaded"
    );
    let json: serde_json::Value =
        serde_json::from_str(&common::stdout(&f.run(&["--json", "update", "--dry-run"]))).unwrap();
    assert_eq!(json["available"], true);
    assert_eq!(json["changes"]["breaking"][0]["version"], next.as_str());
    assert_eq!(json["migrations"][0]["profile"], "default");
}

#[test]
fn air_gapped_update_from_a_bundle() {
    let f = Fixture::self_install();
    let next = next_patch(f.version);
    f.publish(&next, Channel::Stable, &f.bytes(), None);
    let bundle = f.dir.path().join(format!("prometheus-{next}.bundle"));
    let out = f.run(&[
        "bundle",
        "create",
        "--target",
        &f.target,
        "--version",
        &next,
        "-o",
        bundle.to_str().unwrap(),
    ]);
    common::ok(&out);
    let served = f.requests().len();
    // The host goes offline: the release origin is unreachable from now on.
    let config = std::fs::read_to_string(f.home.join("daemon.toml")).unwrap();
    std::fs::write(
        f.home.join("daemon.toml"),
        config.replace(&f.server.uri(), "http://127.0.0.1:9"),
    )
    .unwrap();
    let out = f.run(&["update", "--from-bundle", bundle.to_str().unwrap()]);
    let text = common::ok(&out);
    assert!(
        text.contains(&format!("updated prometheus {} → {next}", f.version)),
        "{text}"
    );
    assert_eq!(f.requests().len(), served, "no network request");
    let receipt = f.latest_receipt();
    assert_eq!(receipt["outcome"], "success");
    assert_eq!(receipt["source"], "bundle");
    let install: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.binary.with_file_name("install.json")).unwrap())
            .unwrap();
    assert_eq!(install["owner"], "bundle");
    assert_eq!(install["version"], next.as_str());
    assert!(f.prev().is_file());
}

#[test]
fn a_bundle_for_another_target_is_refused() {
    let f = Fixture::self_install();
    let other = if f.target == "aarch64-apple-darwin" {
        "x86_64-unknown-linux-musl"
    } else {
        "aarch64-apple-darwin"
    };
    let bundle = f.dir.path().join("other.bundle");
    let description = serde_json::json!({
        "schema": "prometheus.bundle.v1", "version": next_patch(f.version),
        "target": other, "created_at": "2026-11-01T00:00:00Z", "runtimes": [],
    })
    .to_string();
    let mut tar = tar::Builder::new(std::fs::File::create(&bundle).unwrap());
    let mut header = tar::Header::new_ustar();
    header.set_size(description.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "bundle.json", description.as_bytes())
        .unwrap();
    tar.into_inner().unwrap();
    let before = f.bytes();
    let out = f.run(&["update", "--from-bundle", bundle.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", common::stderr(&out));
    assert!(
        common::stderr(&out).contains(&format!(
            "bundle target {other} does not match this host ({})",
            f.target
        )),
        "{}",
        common::stderr(&out)
    );
    assert_eq!(f.bytes(), before);
    assert!(f.requests().is_empty());
}

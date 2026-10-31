//! `prometheus update` against a local release server (spec:
//! distribution-and-update): a copy of the built binary is "installed" with
//! a receipt, and a wiremock server plays the release origin
//! (`update.mirror_url`), serving channel indexes, manifests and archives
//! signed with the test key that debug builds trust.

mod common;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::{NewSession, Source, Store};
use pmt_update::manifest::{Artifact, ChannelIndex, MANIFEST_SCHEMA, ReleaseManifest};
use pmt_update::{Channel, ReleaseVersion};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = include_str!("../../pmt-update/tests/fixtures/test.key");

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    binary: PathBuf,
    server: MockServer,
    runtime: tokio::runtime::Runtime,
    target: String,
    version: ReleaseVersion,
}

impl Fixture {
    /// An install owned by `owner` (`None`: no receipt, a source build).
    fn new(owner: Option<&str>) -> Self {
        let dir = common::temp_home();
        let home = dir.path().join("home");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let binary = bin_dir.join(if cfg!(windows) {
            "prometheus.exe"
        } else {
            "prometheus"
        });
        std::fs::copy(common::BIN, &binary).unwrap();
        let info: serde_json::Value =
            serde_json::from_str(&common::ok(&common::run(&home, &["version", "--json"]))).unwrap();
        let target = info["target"].as_str().unwrap().to_owned();
        let version = ReleaseVersion::parse(info["version"].as_str().unwrap()).unwrap();
        if let Some(owner) = owner {
            let receipt = serde_json::json!({
                "owner": owner, "channel": "stable", "version": version.to_string(),
                "target": target, "installed_at": "2026-10-31T00:00:00Z", "binary": binary,
            });
            std::fs::write(bin_dir.join("install.json"), receipt.to_string()).unwrap();
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        std::fs::write(
            home.join("daemon.toml"),
            format!("[update]\nmirror_url = \"{}\"\n", server.uri()),
        )
        .unwrap();
        Self {
            _dir: dir,
            home,
            binary,
            server,
            runtime,
            target,
            version,
        }
    }

    fn self_install() -> Self {
        Self::new(Some("self"))
    }

    /// Runs the installed copy.
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(&self.binary);
        common::isolate_user_home(&mut cmd, &self.home);
        cmd.arg("--home").arg(&self.home).args(args);
        cmd.stdin(Stdio::null()).output().unwrap()
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.binary).unwrap()
    }

    /// Publishes release `tag` on `channel` with `binary` in its archive;
    /// `served` replaces the archive bytes on the wire (after signing).
    fn publish(&self, tag: &str, channel: Channel, binary: &[u8], served: Option<Vec<u8>>) {
        let version = ReleaseVersion::parse(tag).unwrap();
        let archive = archive(binary);
        let artifact = Artifact::of(&self.target, &archive);
        let manifest = ReleaseManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            version,
            commit: "fixture".to_owned(),
            published_at: "2026-10-31T00:00:00Z".to_owned(),
            artifacts: vec![artifact.clone()],
        };
        let index = serde_json::to_vec(&ChannelIndex::new(channel, version)).unwrap();
        self.serve(&format!("/channels/{channel}.json"), index);
        self.serve(
            &format!("/{tag}/release-manifest.json"),
            manifest.to_published_json().into_bytes(),
        );
        let path = format!("/{tag}/{}", artifact.name);
        self.mount(&format!("{path}.minisig"), sign(&archive).into_bytes());
        self.mount(&path, served.unwrap_or(archive));
    }

    /// Serves `bytes` and their signature at `path`.
    fn serve(&self, path: &str, bytes: Vec<u8>) {
        self.mount(&format!("{path}.minisig"), sign(&bytes).into_bytes());
        self.mount(path, bytes);
    }

    fn mount(&self, at: &str, body: Vec<u8>) {
        self.runtime.block_on(
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(&self.server),
        );
    }

    fn requests(&self) -> Vec<wiremock::Request> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
    }

    fn latest_receipt(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.home.join("updates/receipts/latest.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn prev(&self) -> PathBuf {
        pmt_update::swap::prev_path(&self.binary)
    }

    fn scope(&self, profile: &str) -> ProfileScope {
        let home = Home::open(&self.home).unwrap();
        let dir = home.profile(&ProfileName::parse(profile).unwrap()).unwrap();
        ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap()
    }

    fn create_session(&self, title: &str) {
        let store = Store::open(&self.scope("default")).unwrap();
        let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
        store
            .set_title(session.id, Some(title), pmt_store::TitleSource::User)
            .unwrap();
    }
}

/// `tar.gz` holding `prometheus-<target>/prometheus` as dist lays it out.
fn archive(binary: &[u8]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let name = if cfg!(windows) {
        "prometheus.exe"
    } else {
        "prometheus"
    };
    tar.append_data(&mut header, format!("prometheus-x/{name}"), binary)
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

fn sign(data: &[u8]) -> String {
    let sk = minisign::SecretKeyBox::from_string(TEST_KEY)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, data, None, None)
        .unwrap()
        .into_string()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn next_patch(v: ReleaseVersion) -> String {
    let text = v.to_string();
    let (head, patch) = text.rsplit_once('.').unwrap();
    format!("{head}.{}", patch.parse::<u64>().unwrap() + 1)
}

#[test]
fn check_finds_a_newer_release_and_exits_10_without_identifiers() {
    let f = Fixture::self_install();
    let newer = next_patch(f.version);
    f.publish(&newer, Channel::Stable, b"unused", None);
    let out = f.run(&["update", "--check"]);
    assert_eq!(code(&out), 10, "{}", common::stderr(&out));
    assert_eq!(
        common::stdout(&out).trim(),
        format!("update available: {} → {newer}", f.version)
    );
    let requests = f.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(request.url.query(), None, "no query parameters");
        let agent = request.headers.get("user-agent").unwrap().to_str().unwrap();
        let bare_version = f.version.to_string().trim_start_matches('v').to_owned();
        assert_eq!(agent, format!("prometheus/{bare_version} ({})", f.target));
        for header in ["cookie", "authorization", "x-install-id"] {
            assert!(request.headers.get(header).is_none(), "{header}");
        }
        assert!(
            request.url.path().starts_with("/channels/"),
            "a check reads only the index"
        );
    }
    assert_eq!(f.latest_receipt()["kind"], "check");
}

#[test]
fn every_foreign_owner_is_refused_with_69_before_any_request() {
    for owner in [Some("homebrew"), Some("image"), None] {
        let f = Fixture::new(owner);
        f.publish(&next_patch(f.version), Channel::Stable, b"unused", None);
        let before = f.bytes();
        let out = f.run(&["update"]);
        assert_eq!(code(&out), 69, "{owner:?}: {}", common::stderr(&out));
        let name = owner.unwrap_or("dev");
        assert!(
            common::stderr(&out).contains(&format!("owner: {name}")),
            "{}",
            common::stderr(&out)
        );
        assert_eq!(f.latest_receipt()["outcome"], "refused");
        assert_eq!(f.bytes(), before, "binary unchanged");
        assert!(f.requests().is_empty(), "no request for {name}");
    }
    let f = Fixture::new(Some("homebrew"));
    let out = f.run(&["update", "--check"]);
    assert_eq!(code(&out), 69);
    assert!(common::stderr(&out).contains("update with: brew upgrade prometheus"));
}

#[test]
fn source_build_status_says_it_does_not_self_update() {
    let f = Fixture::new(None);
    let out = f.run(&["update", "--status"]);
    let text = common::ok(&out);
    assert!(text.contains("owner: dev\n"), "{text}");
    assert!(text.contains("this build does not self-update"), "{text}");
    assert!(f.requests().is_empty());
}

#[test]
fn concurrent_update_exits_75_and_changes_nothing() {
    let f = Fixture::self_install();
    f.publish(&next_patch(f.version), Channel::Stable, &f.bytes(), None);
    let held = pmt_update::lock::UpdateLock::acquire(&f.home, &f.binary).unwrap();
    let before = f.bytes();
    let out = f.run(&["update"]);
    assert_eq!(code(&out), 75, "{}", common::stderr(&out));
    let pid = std::process::id();
    assert!(
        common::stderr(&out).contains(&format!("another update is running (pid {pid}, started ")),
        "{}",
        common::stderr(&out)
    );
    assert_eq!(f.bytes(), before);
    assert!(!f.prev().exists());
    drop(held);
}

#[test]
fn tampered_archive_fails_signature_verification_and_keeps_the_binary() {
    let f = Fixture::self_install();
    let newer = next_patch(f.version);
    let mut tampered = archive(&f.bytes());
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    f.publish(&newer, Channel::Stable, &f.bytes(), Some(tampered));
    let before = f.bytes();
    let out = f.run(&["update"]);
    assert_eq!(code(&out), 77, "{}", common::stderr(&out));
    let name = Artifact::file_name(&f.target);
    assert!(
        common::stderr(&out).contains(&format!("signature verification failed for {name}")),
        "{}",
        common::stderr(&out)
    );
    assert_eq!(f.bytes(), before);
    assert_eq!(f.latest_receipt()["outcome"], "failed");
}

#[cfg(unix)]
#[test]
fn broken_build_never_goes_live() {
    let f = Fixture::self_install();
    f.publish(
        &next_patch(f.version),
        Channel::Stable,
        b"#!/bin/sh\necho 'cannot open profile configs' >&2\nexit 1\n",
        None,
    );
    let before = f.bytes();
    let out = f.run(&["update"]);
    assert_eq!(code(&out), 1, "{}", common::stderr(&out));
    assert!(
        common::stderr(&out).contains("staged build failed self-test: cannot open profile configs"),
        "{}",
        common::stderr(&out)
    );
    assert_eq!(f.bytes(), before);
    assert_eq!(f.latest_receipt()["outcome"], "failed");
    assert!(
        std::fs::read_dir(f.home.join("updates/staging"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn update_then_rollback_restores_the_previous_binary() {
    let f = Fixture::self_install();
    let newer = next_patch(f.version);
    f.publish(&newer, Channel::Stable, &f.bytes(), None);
    let out = f.run(&["-p", "default", "update"]);
    let text = common::ok(&out);
    assert!(
        text.contains(&format!("updated prometheus {} → {newer}", f.version)),
        "{text}"
    );
    let receipt = f.latest_receipt();
    assert_eq!(receipt["outcome"], "success");
    assert_eq!(receipt["to"], newer.as_str());
    let stages: Vec<&str> = receipt["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        stages,
        [
            "check",
            "download",
            "verify",
            "unpack",
            "self-test",
            "snapshot",
            "swap"
        ]
    );
    assert!(f.prev().is_file());
    let install: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.binary.with_file_name("install.json")).unwrap())
            .unwrap();
    assert_eq!(install["version"], newer.as_str());
    let snapshots = common::ok(&f.run(&["snapshots", "list"]));
    assert!(snapshots.contains("pre-update"), "{snapshots}");
    assert!(snapshots.contains(&format!("pre-{newer}")), "{snapshots}");

    let out = f.run(&["update", "--rollback"]);
    let text = common::ok(&out);
    assert!(
        text.contains(&format!("rolled back to {}", f.version)),
        "{text}"
    );
    assert_eq!(f.latest_receipt()["outcome"], "rolled_back");
    let install: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.binary.with_file_name("install.json")).unwrap())
            .unwrap();
    assert_eq!(install["version"], f.version.to_string());
    assert!(common::ok(&f.run(&["version"])).contains(&f.version.to_string()[1..]));
}

#[test]
fn rollback_after_a_store_migration_restores_the_old_store() {
    let f = Fixture::self_install();
    f.create_session("before the update");
    let newer = next_patch(f.version);
    f.publish(&newer, Channel::Stable, &f.bytes(), None);
    common::ok(&f.run(&["update"]));
    // What the new build does on first open: a session, then a migration
    // the previous build cannot read.
    f.create_session("after the update");
    let migrated = pmt_store::SCHEMA_VERSION + 1;
    let db = f.home.join("profiles/default/state.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(&format!(
            "UPDATE store_meta SET value = '{migrated}' WHERE key IN ('schema_version', 'min_read_version');"
        ))
        .unwrap();

    let out = f.run(&["update", "--rollback"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        common::stderr(&out).lines().next().unwrap(),
        format!(
            "error: cannot roll back: profile default state.db schema {migrated} is newer than {} supports (max {}); rerun with --restore-snapshots to restore the pre-update snapshot",
            f.version,
            pmt_store::SCHEMA_VERSION
        )
    );

    // Older binary meets newer schema: refused, database untouched.
    let before = std::fs::read(&db).unwrap();
    let out = f.run(&["sessions", "list"]);
    assert_eq!(code(&out), 1, "{}", common::stderr(&out));
    assert!(common::stderr(&out).contains(&format!(
        "state.db schema {migrated} is newer than this build supports ({}); update prometheus or roll back with --restore-snapshots",
        pmt_store::SCHEMA_VERSION
    )));
    assert_eq!(std::fs::read(&db).unwrap(), before);

    let out = f.run(&["update", "--rollback", "--restore-snapshots"]);
    assert_eq!(
        code(&out),
        4,
        "no confirmation without a TTY or --yes: {}",
        common::stderr(&out)
    );
    assert!(
        common::stderr(&out).contains("loses 1 session(s) created since the update"),
        "{}",
        common::stderr(&out)
    );

    let out = f.run(&["update", "--rollback", "--restore-snapshots", "--yes"]);
    let text = common::ok(&out);
    assert!(
        text.contains(&format!("rolled back to {}", f.version)),
        "{text}"
    );
    let listed = common::ok(&f.run(&["sessions", "list"]));
    assert!(listed.contains("before the update"), "{listed}");
    assert!(!listed.contains("after the update"), "{listed}");
}

#[test]
fn receipts_stay_at_the_root_home_with_a_profile_selected() {
    let f = Fixture::self_install();
    common::ok(&f.run(&["profile", "create", "work"]));
    f.publish(&next_patch(f.version), Channel::Stable, b"unused", None);
    let out = f.run(&["-p", "work", "update", "--check"]);
    assert_eq!(code(&out), 10);
    assert!(f.home.join("updates/receipts/latest.json").is_file());
    assert!(!f.home.join("profiles/work/updates").exists());
}

#[test]
fn newer_build_than_the_channel_stays_and_downloads_nothing() {
    let f = Fixture::self_install();
    // A build ahead of its channel (as a canary is after switching to
    // stable) is never downgraded without --allow-downgrade.
    f.publish("v0.0.1", Channel::Stable, b"unused", None);
    let out = f.run(&["update"]);
    let text = common::ok(&out);
    assert!(
        text.contains("current build is newer than stable v0.0.1; staying until stable catches up"),
        "{text}"
    );
    assert!(
        f.requests()
            .iter()
            .all(|r| r.url.path().starts_with("/channels/"))
    );
}

#[test]
fn set_channel_persists_in_the_receipt_and_channel_applies_once() {
    let f = Fixture::self_install();
    let canary = format!("{}-canary.20261029064100", next_patch(f.version));
    f.publish(&canary, Channel::Canary, b"unused", None);
    let out = f.run(&["update", "--check", "--channel", "canary"]);
    assert_eq!(code(&out), 10);
    let receipt = |f: &Fixture| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(f.binary.with_file_name("install.json")).unwrap())
            .unwrap()
    };
    assert_eq!(receipt(&f)["channel"], "stable", "--channel is for one run");
    assert_eq!(
        common::ok(&f.run(&["update", "--set-channel", "canary"])).trim(),
        "channel: canary"
    );
    assert_eq!(receipt(&f)["channel"], "canary");
    assert_eq!(code(&f.run(&["update", "--check"])), 10);
}

#[test]
fn unknown_channel_is_a_usage_error() {
    let f = Fixture::self_install();
    let out = f.run(&["update", "--channel", "main"]);
    assert_eq!(code(&out), 2);
    assert!(common::stderr(&out).contains("--channel must be stable or canary, not main"));
    assert!(f.requests().is_empty());
}

//! The desktop updater (spec: desktop-app, Signed in-app updates;
//! Development builds have no updater) against an in-memory release
//! origin signed with the test key.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pmt_desktop::update::{
    Archive, FEED_SCHEMA, Fetch, Fetching, IN_PROGRESS, Mechanism, NO_UPDATES, SIGNATURE_INVALID,
    Updater, UpdaterConfig,
};
use serde_json::json;

const TEST_KEY: &str = include_str!("../../pmt-update/tests/fixtures/test.key");
const TEST_PUB: &str = include_str!("../../pmt-update/keys/test.pub");
const ORIGIN: &str = "https://releases.example";
const ARCHIVE: &str = "https://releases.example/v1.6.0/prometheus-desktop-x86_64-linux.AppImage";

fn sign(data: &[u8]) -> Vec<u8> {
    let sk = minisign::SecretKeyBox::from_string(TEST_KEY)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, data, None, None)
        .unwrap()
        .into_string()
        .into_bytes()
}

/// Serves files from memory and records every URL asked for; a `gate`
/// holds downloads of the archive until it is opened.
#[derive(Default)]
struct Origin {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    asked: Mutex<Vec<String>>,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Origin {
    fn publish(&self, url: &str, bytes: &[u8], served: Option<&[u8]>) {
        let mut files = self.files.lock().unwrap();
        files.insert(format!("{url}.minisig"), sign(bytes));
        files.insert(url.to_owned(), served.unwrap_or(bytes).to_vec());
    }

    fn release(&self, archive: &[u8], served: Option<&[u8]>) {
        self.release_listing(archive, archive, served);
    }

    /// Publishes 1.6.0 whose signed feed names `listed`'s digest and whose
    /// URL serves `archive` (signed by the release key either way).
    fn release_listing(&self, listed: &[u8], archive: &[u8], served: Option<&[u8]>) {
        let entry = Archive::of("prometheus-desktop-x86_64-linux.AppImage", listed);
        let feed = json!({
            "schema": FEED_SCHEMA,
            "version": "1.6.0",
            "notes": "faster startup",
            "platforms": {"linux-x86_64": entry},
        });
        self.publish(
            &format!("{ORIGIN}/channels/desktop-stable.json"),
            feed.to_string().as_bytes(),
            None,
        );
        self.publish(ARCHIVE, archive, served);
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

struct Shared(Arc<Origin>);

impl Fetch for Shared {
    fn get<'a>(&'a self, url: &'a str) -> Fetching<'a> {
        Box::pin(async move {
            self.0.asked.lock().unwrap().push(url.to_owned());
            if url == ARCHIVE
                && let Some(gate) = &self.0.gate
            {
                gate.notified().await;
            }
            self.0
                .files
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("404 {url}"))
        })
    }
}

fn config(dir: &std::path::Path, key: Option<&str>) -> UpdaterConfig {
    UpdaterConfig {
        release_key: key.map(str::to_owned),
        channel: "stable".into(),
        origin: ORIGIN.into(),
        platform: "linux-x86_64".into(),
        current: "1.5.0".into(),
        dir: dir.to_owned(),
    }
}

fn updater(origin: &Arc<Origin>, dir: &std::path::Path) -> Updater {
    Updater::new(
        config(dir, Some(TEST_PUB)),
        Box::new(Shared(Arc::clone(origin))),
    )
}

/// Scenario: Bad signature.
#[tokio::test]
async fn a_tampered_update_is_discarded_and_the_current_version_kept() {
    let dir = tempfile::tempdir().unwrap();
    let origin = Arc::new(Origin::default());
    origin.release(b"the real app", Some(b"the real app, tampered"));
    let updater = updater(&origin, dir.path());
    let checked = updater.check(true).await;
    assert_eq!(checked.available.unwrap().version, "1.6.0");
    let applied = updater.apply().await;
    assert!(!applied.ok);
    assert_eq!(applied.message.as_deref(), Some(SIGNATURE_INVALID));
    assert_eq!(applied.staged, None);
    assert_eq!(updater.staged(), None, "nothing is staged");
    assert!(
        !dir.path().join("staged").exists(),
        "nothing reached the disk"
    );
}

/// An older build signed by the release key, served at the new version's
/// URL, is not the archive the signed feed names: it is discarded rather
/// than installed as the new version.
#[tokio::test]
async fn an_older_signed_archive_under_the_new_url_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let origin = Arc::new(Origin::default());
    origin.release_listing(b"the 1.6.0 app", b"the 1.4.0 app", None);
    let updater = updater(&origin, dir.path());
    assert_eq!(
        updater.check(true).await.available.unwrap().version,
        "1.6.0"
    );
    let applied = updater.apply().await;
    assert!(!applied.ok, "{applied:?}");
    assert_eq!(applied.message.as_deref(), Some(SIGNATURE_INVALID));
    assert_eq!(updater.staged(), None, "nothing is staged");
    assert!(!dir.path().join("staged").exists());
}

#[tokio::test]
async fn a_verified_update_is_staged_for_the_next_restart() {
    let dir = tempfile::tempdir().unwrap();
    let origin = Arc::new(Origin::default());
    origin.release(b"the real app", None);
    let updater = updater(&origin, dir.path());
    assert!(
        updater
            .apply()
            .await
            .message
            .unwrap()
            .contains("check for updates first")
    );
    assert!(
        !origin.asked().contains(&ARCHIVE.to_owned()),
        "no download before an offer is accepted"
    );
    updater.check(true).await;
    let applied = updater.apply().await;
    assert!(applied.ok, "{applied:?}");
    let staged = updater.staged().unwrap();
    assert_eq!(staged.version, "1.6.0");
    assert_eq!(std::fs::read(&staged.archive).unwrap(), b"the real app");
}

/// Scenario: Apply on a commit build.
#[tokio::test]
async fn a_build_without_the_release_key_has_no_updater_and_asks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let origin = Arc::new(Origin::default());
    origin.release(b"the real app", None);
    let updater = Updater::new(
        config(dir.path(), None),
        Box::new(Shared(Arc::clone(&origin))),
    );
    assert_eq!(updater.mechanism(), Mechanism::None);
    for outcome in [updater.check(true).await, updater.apply().await] {
        assert_eq!(outcome.mechanism, Mechanism::None);
        assert_eq!(outcome.message.as_deref(), Some(NO_UPDATES));
    }
    assert!(
        origin.asked().is_empty(),
        "no network request: {:?}",
        origin.asked()
    );
}

#[tokio::test]
async fn a_second_apply_while_one_runs_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let origin = Arc::new(Origin {
        gate: Some(Arc::clone(&gate)),
        ..Origin::default()
    });
    origin.release(b"the real app", None);
    let updater = Arc::new(updater(&origin, dir.path()));
    updater.check(true).await;
    let first = pmt_core::spawn::spawn({
        let updater = Arc::clone(&updater);
        async move { updater.apply().await }
    });
    while !origin.asked().contains(&ARCHIVE.to_owned()) {
        tokio::task::yield_now().await;
    }
    let second = updater.apply().await;
    assert_eq!(second.message.as_deref(), Some(IN_PROGRESS));
    gate.notify_one();
    assert!(first.await.unwrap().ok);
}

#[tokio::test]
async fn background_checks_ask_the_channel_at_most_every_six_hours() {
    let dir = tempfile::tempdir().unwrap();
    let origin = Arc::new(Origin::default());
    origin.release(b"the real app", None);
    let updater = updater(&origin, dir.path());
    assert!(updater.check(false).await.available.is_some());
    let asked = origin.asked().len();
    let again = updater.check(false).await;
    assert!(again.available.is_some(), "the offer is remembered");
    assert_eq!(origin.asked().len(), asked, "no request within 6 hours");
    // A fresh updater (an app restart) reads the last check time too.
    let restarted = self::updater(&origin, dir.path());
    restarted.check(false).await;
    assert_eq!(origin.asked().len(), asked);
}

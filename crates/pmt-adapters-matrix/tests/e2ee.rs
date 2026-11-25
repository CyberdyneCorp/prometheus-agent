//! Matrix end-to-end encryption against the mock homeserver and another
//! user's vodozemac device (spec: platform-adapters, Matrix end-to-end
//! encryption): keys published, room keys taken over Olm, Megolm events
//! decrypted and replies encrypted for the member's device, the store
//! surviving a restart, attachments encrypted, cross-signing from secret
//! storage, undecryptable events dropped, and required encryption failing
//! closed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_matrix::mock::{BOT, BOT_DEVICE, DM, MatrixMock, TOKEN, USER};
use pmt_adapters_matrix::mock_keys::{Device, recovery_key_text, seal_attachment, verify_signed};
use pmt_adapters_matrix::{E2EE_UNAVAILABLE, Matrix, RECOVERY_SECRET, TOKEN_SECRET};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, Outgoing,
    SendError,
};
use pmt_store::gateway::outbox::OutboundAttachment;
use serde_json::{Value, json};

const USER_DEVICE: &str = "KITPHONE";

#[derive(Default)]
struct Link {
    events: Mutex<Vec<Inbound>>,
}

impl AdapterLink for Link {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }
    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}
}

impl Link {
    fn texts(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Rig {
    home: tempfile::TempDir,
    mock: Arc<MatrixMock>,
}

impl Rig {
    async fn new() -> Self {
        let mock = MatrixMock::start().await;
        mock.encrypt_dm();
        Self {
            home: tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
            mock,
        }
    }

    /// A fresh adapter of the rig's profile with `options` and `secrets`.
    fn adapter(&self, options: &str, secrets: &[(&str, &str)]) -> Matrix {
        let table = format!(
            "options = {{ homeserver = \"{}\"{options} }}",
            self.mock.homeserver()
        );
        let mut all = vec![(TOKEN_SECRET, TOKEN)];
        all.extend_from_slice(secrets);
        let setup = kit_setup(self.home.path(), "matrix", &table, &all).unwrap();
        Matrix::new(&setup).unwrap()
    }

    fn store(&self) -> std::path::PathBuf {
        self.home
            .path()
            .join("profiles/default/platforms/matrix/crypto")
    }

    /// The events the bot sent of type `kind`.
    fn sent(&self, kind: &str) -> Vec<Value> {
        self.mock
            .events()
            .into_iter()
            .filter(|(_, k, _)| k == kind)
            .map(|(_, _, c)| c)
            .collect()
    }
}

async fn connected(adapter: &Matrix) -> Arc<Link> {
    let link = Arc::new(Link::default());
    adapter
        .connect(Arc::clone(&link) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    link
}

#[tokio::test]
async fn an_encrypted_message_is_decrypted_and_the_reply_encrypted_for_the_senders_device() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let bot = rig.adapter("", &[]);
    let link = connected(&bot).await;
    // The device keys are published, self-signed, with one-time keys.
    let published = keys
        .device_keys(BOT, BOT_DEVICE)
        .expect("device keys uploaded");
    let ed = published["keys"][format!("ed25519:{BOT_DEVICE}")]
        .as_str()
        .unwrap();
    assert!(verify_signed(&published, BOT, BOT_DEVICE, ed));
    assert!(keys.one_time_key_count(BOT, BOT_DEVICE) > 0);
    phone.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    let encrypted = phone.encrypt_text(DM, "the launch code is 1234");
    rig.mock.push_encrypted(&encrypted);
    eventually(|| link.texts() == ["the launch code is 1234"]).await;
    bot.send(&Outgoing::text(DM, None, "noted")).await.unwrap();
    let sent = rig.sent("m.room.encrypted");
    assert_eq!(sent.len(), 1, "{:?}", rig.mock.events());
    assert!(
        rig.sent("m.room.message").is_empty(),
        "nothing in the clear"
    );
    assert!(sent[0].get("body").is_none());
    phone.receive(keys);
    let clear = phone
        .decrypt(&sent[0])
        .expect("the phone holds the room key");
    assert_eq!(clear["room_id"], DM);
    assert_eq!(clear["content"]["body"], "noted");
    bot.disconnect().await;
}

#[tokio::test]
async fn the_store_survives_a_restart_and_keeps_its_room_keys() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let first = rig.adapter("", &[]);
    let link = connected(&first).await;
    phone.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    rig.mock.push_encrypted(&phone.encrypt_text(DM, "one"));
    eventually(|| link.texts() == ["one"]).await;
    first.disconnect().await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(rig.store()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    let store = std::fs::read_to_string(rig.store().join("store.json")).unwrap();
    // Plain vodozemac pickles name their keys; encrypted ones do not.
    assert!(
        !store.contains("signing_key") && !store.contains("diffie_hellman"),
        "pickles are encrypted"
    );
    let second = rig.adapter("", &[]);
    let link = connected(&second).await;
    rig.mock.push_encrypted(&phone.encrypt_text(DM, "two"));
    eventually(|| link.texts() == ["two"]).await;
    // The same device: its keys were not uploaded again.
    second.disconnect().await;
}

#[tokio::test]
async fn an_event_that_cannot_be_decrypted_is_dropped_never_passed_on() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut stranger = Device::publish(keys, USER, "NOKEYS");
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let bot = rig.adapter("", &[]);
    let link = connected(&bot).await;
    // The stranger never shared its room key.
    rig.mock
        .push_encrypted(&stranger.encrypt_text(DM, "unreadable"));
    phone.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    rig.mock.push_encrypted(&phone.encrypt_text(DM, "readable"));
    eventually(|| !link.texts().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(link.texts(), ["readable"]);
    bot.disconnect().await;
}

/// A session speaks only for the user who shared it: the homeserver
/// cannot relabel its events as another (allowlisted) user's.
#[tokio::test]
async fn a_session_decrypts_only_events_of_the_user_who_shared_it() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let bot = rig.adapter("", &[]);
    let link = connected(&bot).await;
    phone.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    rig.mock
        .push_encrypted_as("@admin:example.org", &phone.encrypt_text(DM, "relabelled"));
    rig.mock.push_encrypted(&phone.encrypt_text(DM, "genuine"));
    eventually(|| !link.texts().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(link.texts(), ["genuine"]);
    bot.disconnect().await;
}

/// A room key from a device that is not in the named user's device list
/// is refused, so its messages are never attributed to that user.
#[tokio::test]
async fn a_room_key_from_a_device_the_user_does_not_own_is_refused() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut impostor = Device::unlisted(USER, "IMPOSTOR");
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let bot = rig.adapter("", &[]);
    let link = connected(&bot).await;
    impostor.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    rig.mock
        .push_encrypted(&impostor.encrypt_text(DM, "forged"));
    phone.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    rig.mock.push_encrypted(&phone.encrypt_text(DM, "genuine"));
    eventually(|| !link.texts().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(link.texts(), ["genuine"]);
    bot.disconnect().await;
}

#[tokio::test]
async fn files_go_into_an_encrypted_room_as_encrypted_attachments_both_ways() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let bot = rig.adapter("", &[]);
    let link = connected(&bot).await;
    // Outbound: the upload is ciphertext, the event carries `file`.
    let file = rig.home.path().join("report.pdf");
    std::fs::write(&file, b"quarterly numbers").unwrap();
    let mut message = Outgoing::text(DM, None, "");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    bot.send(&message).await.unwrap();
    phone.receive(keys);
    let sent = rig.sent("m.room.encrypted");
    let clear = phone.decrypt(sent.last().unwrap()).unwrap();
    let info = &clear["content"]["file"];
    assert!(clear["content"].get("url").is_none(), "{clear}");
    let stored = rig.mock.media(info["url"].as_str().unwrap()).unwrap();
    assert_ne!(stored, b"quarterly numbers");
    // Inbound: the phone's encrypted attachment opens as plaintext.
    let (ciphertext, mut sealed) = seal_attachment(b"holiday photo");
    sealed["url"] = json!(rig.mock.add_media("enc-photo", ciphertext));
    phone.share_room_key(keys, DM, BOT, BOT_DEVICE);
    rig.mock.wake();
    let content = json!({"msgtype": "m.image", "body": "photo.jpg", "file": sealed});
    rig.mock.push_encrypted(&phone.encrypt(DM, &content));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let attachment = link.events.lock().unwrap()[0].attachments[0].clone();
    let mut stream = bot.open_media(&attachment).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, b"holiday photo");
    bot.disconnect().await;
}

#[tokio::test]
async fn the_device_is_cross_signed_from_secret_storage_on_every_start() {
    let rig = Rig::new().await;
    let recovery = [42u8; 32];
    let ssk = rig.mock.keys().cross_signing(BOT, &recovery);
    let text = recovery_key_text(&recovery);
    for start in 1..=2 {
        let bot = rig.adapter("", &[(RECOVERY_SECRET, &text)]);
        connected(&bot).await;
        let uploads = rig.mock.keys().signature_uploads();
        assert_eq!(uploads.len(), start);
        let signed = &uploads[start - 1][BOT][BOT_DEVICE];
        assert!(verify_signed(signed, BOT, &ssk, &ssk), "{signed}");
        bot.disconnect().await;
    }
    // A wrong recovery key signs nothing; encryption still runs.
    let wrong = recovery_key_text(&[1u8; 32]);
    let bot = rig.adapter("", &[(RECOVERY_SECRET, &wrong)]);
    connected(&bot).await;
    assert_eq!(rig.mock.keys().signature_uploads().len(), 2);
    bot.disconnect().await;
}

/// Scenario: Required encryption fails closed.
#[tokio::test]
async fn required_encryption_with_a_corrupted_store_fails_closed_and_reads_no_rooms() {
    let rig = Rig::new().await;
    let first = rig.adapter("", &[]);
    connected(&first).await;
    first.disconnect().await;
    let syncs = rig.mock.syncs();
    std::fs::write(rig.store().join("store.json"), b"{\"account\": 17").unwrap();
    let bot = rig.adapter(", e2ee = \"required\"", &[]);
    let err = bot.connect(Arc::new(Link::default())).await.unwrap_err();
    assert_eq!(err.code, E2EE_UNAVAILABLE);
    assert!(!err.retryable, "fatal: {err:?}");
    assert!(err.message.contains("corrupted"), "{}", err.message);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.mock.syncs(), syncs, "no room was read");
}

#[tokio::test]
async fn optional_encryption_that_cannot_start_reads_and_writes_no_encrypted_room() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let mut phone = Device::publish(keys, USER, USER_DEVICE);
    let first = rig.adapter("", &[]);
    connected(&first).await;
    first.disconnect().await;
    std::fs::write(rig.store().join("store.json"), b"not json").unwrap();
    let bot = rig.adapter("", &[]);
    let link = connected(&bot).await;
    let refused = bot.send(&Outgoing::text(DM, None, "hi")).await.unwrap_err();
    assert!(
        matches!(&refused, SendError::Permanent(why) if why.contains("encryption is not running")),
        "{refused:?}"
    );
    assert!(
        rig.sent("m.room.message").is_empty(),
        "nothing in the clear"
    );
    rig.mock.push_encrypted(&phone.encrypt_text(DM, "unseen"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(link.texts().is_empty());
    bot.disconnect().await;
}

#[tokio::test]
async fn a_store_whose_one_time_keys_no_longer_match_the_server_is_refused() {
    let rig = Rig::new().await;
    let keys = rig.mock.keys();
    let first = rig.adapter("", &[]);
    connected(&first).await;
    first.disconnect().await;
    let backup = std::fs::read(rig.store().join("store.json")).unwrap();
    // Others claim keys; a later start publishes new ones.
    keys.claim_oldest(BOT, BOT_DEVICE, 10);
    let second = rig.adapter("", &[]);
    connected(&second).await;
    second.disconnect().await;
    // The store goes back to the backup, which reuses those key ids.
    std::fs::write(rig.store().join("store.json"), backup).unwrap();
    keys.claim_oldest(BOT, BOT_DEVICE, 5);
    let bot = rig.adapter(", e2ee = \"required\"", &[]);
    let err = bot.connect(Arc::new(Link::default())).await.unwrap_err();
    assert_eq!(err.code, E2EE_UNAVAILABLE);
    assert!(
        err.message.contains("one-time keys")
            && err.message.contains("no longer matches the homeserver")
            && err.message.contains("delete"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn encryption_off_leaves_encrypted_rooms_alone() {
    let rig = Rig::new().await;
    let bot = rig.adapter(", e2ee = \"off\"", &[]);
    connected(&bot).await;
    assert!(rig.mock.keys().device_keys(BOT, BOT_DEVICE).is_none());
    assert!(bot.send(&Outgoing::text(DM, None, "hi")).await.is_err());
    bot.disconnect().await;
}

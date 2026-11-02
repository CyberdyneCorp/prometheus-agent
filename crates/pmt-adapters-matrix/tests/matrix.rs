//! The Matrix adapter against a mock homeserver (spec: platform-adapters):
//! the conformance kit, the kept `/sync` cursor, invites, media.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_matrix::mock::{self, MatrixMock, TOKEN};
use pmt_adapters_matrix::{Matrix, TOKEN_SECRET};
use pmt_gateway::messaging::conformance::{self, MockPlatform as _, kit_setup};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, MediaKind,
    Outgoing, Reaction,
};
use pmt_store::gateway::outbox::OutboundAttachment;
use serde_json::json;

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Inbound>>,
}

impl AdapterLink for Recorder {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn adapter(home: &std::path::Path, mock: &MatrixMock, extra: &str) -> Matrix {
    let table = format!("{}\n{extra}", mock.table());
    let setup = kit_setup(home, "matrix", &table, &[(TOKEN_SECRET, TOKEN)]).unwrap();
    Matrix::new(&setup).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_passes_the_conformance_kit() {
    let home = tempfile::tempdir().unwrap();
    let (adapter, platform) = mock::kit(home.path()).await.unwrap();
    let report = conformance::run(adapter, platform).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
}

#[tokio::test]
async fn history_is_not_answered_and_the_cursor_survives_reconnects() {
    let home = tempfile::tempdir().unwrap();
    let mock = MatrixMock::start().await;
    mock.inject("old history");
    let matrix = adapter(home.path(), &mock, "");
    let link = Arc::new(Recorder::default());
    matrix.connect(Arc::clone(&link) as _).await.unwrap();
    mock.inject("hello");
    eventually(|| link.events.lock().unwrap().len() == 1).await;
    matrix.disconnect().await;
    for text in ["one", "two", "three"] {
        mock.inject(text);
    }
    matrix.connect(Arc::clone(&link) as _).await.unwrap();
    eventually(|| link.events.lock().unwrap().len() == 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let texts: Vec<String> = link
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.text.clone())
        .collect();
    assert_eq!(texts, ["hello", "one", "two", "three"]);
}

#[tokio::test]
async fn invites_are_accepted_only_from_allowed_users() {
    let home = tempfile::tempdir().unwrap();
    let mock = MatrixMock::start().await;
    let matrix = adapter(home.path(), &mock, "allow_users = [\"@kit:kit\"]");
    matrix.connect(Arc::new(Recorder::default())).await.unwrap();
    mock.invite("!stranger:kit", "@stranger:kit");
    mock.invite("!ops:kit", "@kit:kit");
    eventually(|| !mock.joined().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.joined(), ["!ops:kit"]);
    matrix.disconnect().await;
}

#[tokio::test]
async fn media_downloads_and_uploads_and_reactions_are_redacted_to_remove() {
    let home = tempfile::tempdir().unwrap();
    let mock = MatrixMock::start().await;
    let uri = mock.add_media("cat", vec![1, 2, 3]);
    let matrix = adapter(home.path(), &mock, "");
    let link = Arc::new(Recorder::default());
    matrix.connect(Arc::clone(&link) as _).await.unwrap();
    mock.push(&json!({"msgtype": "m.image", "body": "cat.png", "url": uri}));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.attachments[0].kind, MediaKind::Image);
    let mut stream = matrix.open_media(&event.attachments[0]).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, [1, 2, 3]);
    let file = home.path().join("chart.png");
    std::fs::write(&file, b"PNG").unwrap();
    let mut message = Outgoing::text(mock::DM, None, "");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    assert!(matrix.send(&message).await.unwrap().message_id.is_some());
    let mut reaction = Reaction {
        chat_id: mock::DM.to_owned(),
        message_id: event.message_id.clone(),
        emoji: "\u{1f440}".to_owned(),
        remove: false,
    };
    matrix.react(&reaction).await.unwrap();
    reaction.remove = true;
    matrix.react(&reaction).await.unwrap();
    assert_eq!(mock.redactions(), 1);
    matrix.disconnect().await;
}

#[tokio::test]
async fn a_missing_homeserver_fails_validation_naming_it() {
    let home = tempfile::tempdir().unwrap();
    let err = kit_setup(home.path(), "matrix", "", &[(TOKEN_SECRET, TOKEN)]).unwrap_err();
    assert!(
        err.message()
            .contains("gateway.platforms.matrix.options.homeserver"),
        "{}",
        err.message()
    );
}

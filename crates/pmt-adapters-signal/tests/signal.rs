//! The Signal adapter against a mock signal-cli daemon (spec:
//! platform-adapters): the conformance kit, an unreachable daemon, missing
//! settings, attachments in and out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_signal::Signal;
use pmt_adapters_signal::mock::{self, SignalMock};
use pmt_gateway::messaging::conformance::{self, kit_setup};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, MediaKind,
    Outgoing,
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

#[tokio::test(flavor = "multi_thread")]
async fn the_adapter_passes_the_conformance_kit() {
    let home = tempfile::tempdir().unwrap();
    let (adapter, platform) = mock::kit(home.path()).await.unwrap();
    let report = conformance::run(adapter, platform).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
}

#[tokio::test]
async fn an_unreachable_daemon_is_retried_naming_its_url() {
    let home = tempfile::tempdir().unwrap();
    // A port nothing listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let table = format!(
        "options = {{ http_url = \"http://127.0.0.1:{port}\", account = \"+15550000001\" }}"
    );
    let setup = kit_setup(home.path(), "signal", &table, &[]).unwrap();
    let err = Signal::new(&setup)
        .unwrap()
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "signal_daemon_unreachable");
    assert!(err.retryable);
    assert!(
        err.message.contains(&format!("http://127.0.0.1:{port}")),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn a_missing_account_fails_validation_naming_it() {
    let home = tempfile::tempdir().unwrap();
    let err = kit_setup(
        home.path(),
        "signal",
        "options = { http_url = \"http://127.0.0.1:1\" }",
        &[],
    )
    .unwrap_err();
    assert!(
        err.message()
            .contains("gateway.platforms.signal.options.account"),
        "{}",
        err.message()
    );
}

#[tokio::test]
async fn attachments_download_and_upload() {
    let home = tempfile::tempdir().unwrap();
    let mock = SignalMock::start().await;
    mock.add_attachment("att-1", vec![5, 6, 7]);
    let setup = kit_setup(home.path(), "signal", &mock.table(), &[]).unwrap();
    let signal = Signal::new(&setup).unwrap();
    let link = Arc::new(Recorder::default());
    signal.connect(Arc::clone(&link) as _).await.unwrap();
    mock.receive(&json!({"message": "", "attachments": [{"id": "att-1", "contentType": "image/jpeg", "filename": "a.jpg"}]}));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.attachments[0].kind, MediaKind::Image);
    let mut stream = signal.open_media(&event.attachments[0]).await.unwrap();
    let chunk = futures_util::StreamExt::next(&mut stream.chunks)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk, [5, 6, 7]);
    let file = home.path().join("chart.png");
    std::fs::write(&file, b"PNG").unwrap();
    let mut message = Outgoing::text(mock::USER, None, "the chart");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    signal.send(&message).await.unwrap();
    assert_eq!(mock.uploads(), [file.to_string_lossy().into_owned()]);
    signal.disconnect().await;
}

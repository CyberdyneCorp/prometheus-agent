//! The email adapter against a mock IMAP and SMTP server (spec:
//! platform-adapters): the conformance kit, old mail left unanswered,
//! threaded replies, attachments, missing settings.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_email::mock::{self, MAILBOX, MailMock, PASSWORD, USER};
use pmt_adapters_email::{Email, PASSWORD_SECRET};
use pmt_gateway::messaging::conformance::{self, kit_setup};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, MediaKind,
    Outgoing,
};
use pmt_store::gateway::outbox::OutboundAttachment;

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

fn email(home: &std::path::Path, mock: &MailMock) -> Email {
    let setup = kit_setup(home, "email", &mock.table(), &[(PASSWORD_SECRET, PASSWORD)]).unwrap();
    Email::new(&setup).unwrap()
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
async fn mail_already_in_the_inbox_is_not_answered() {
    let home = tempfile::tempdir().unwrap();
    let mock = MailMock::start().await;
    mock.receive("Old", "", "from last week");
    let adapter = email(home.path(), &mock);
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    assert_eq!(mock.unseen(), 0);
    mock.receive("New question", "", "What is up?");
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let events = link.events.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].text, "[Subject: New question]\n\nWhat is up?");
    adapter.disconnect().await;
}

#[tokio::test]
async fn threaded_reply() {
    let home = tempfile::tempdir().unwrap();
    let mock = MailMock::start().await;
    let adapter = email(home.path(), &mock);
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    let id = mock.receive(
        "Re: Re: Budget",
        "References: <root@kit.example>\r\n",
        "numbers?",
    );
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let mut reply = Outgoing::text(USER, None, "Here they are.");
    reply.reply_to = Some(id.clone());
    adapter.send(&reply).await.unwrap();
    let raw = String::from_utf8(mock.raw_sent()[0].clone()).unwrap();
    assert!(raw.contains(&format!("In-Reply-To: {id}\r\n")), "{raw}");
    assert!(
        raw.contains(&format!("References: <root@kit.example> {id}\r\n")),
        "{raw}"
    );
    assert!(raw.contains("Subject: Re: Budget\r\n"), "{raw}");
    assert!(raw.contains(&format!("From: {MAILBOX}\r\n")), "{raw}");
    adapter.disconnect().await;
}

#[tokio::test]
async fn attachments_arrive_and_go_out() {
    let home = tempfile::tempdir().unwrap();
    let mock = MailMock::start().await;
    let adapter = email(home.path(), &mock);
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    let raw = format!(
        "From: {USER}\r\nTo: {MAILBOX}\r\nSubject: Re: pic\r\nMessage-ID: <att@kit.example>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n--b\r\nContent-Type: image/png; name=\"cat.png\"\r\nContent-Disposition: attachment; filename=\"cat.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\nAQID\r\n--b--\r\n"
    );
    mock.deliver(raw.as_bytes());
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.text, "see attached");
    assert_eq!(event.attachments[0].kind, MediaKind::Image);
    assert_eq!(event.attachments[0].name.as_deref(), Some("cat.png"));
    let mut stream = adapter.open_media(&event.attachments[0]).await.unwrap();
    let chunk = futures_util::StreamExt::next(&mut stream.chunks)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk, [1, 2, 3]);
    let file = home.path().join("report.pdf");
    std::fs::write(&file, b"%PDF").unwrap();
    let mut message = Outgoing::text(USER, None, "the report");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    adapter.send(&message).await.unwrap();
    let sent = String::from_utf8(mock.raw_sent()[0].clone()).unwrap();
    assert!(sent.contains("filename=\"report.pdf\""), "{sent}");
    adapter.disconnect().await;
}

#[tokio::test]
async fn a_missing_imap_host_is_named() {
    let home = tempfile::tempdir().unwrap();
    let table = "options = { address = \"a@b.example\", smtp_host = \"smtp.b.example\" }";
    let setup = kit_setup(home.path(), "email", table, &[(PASSWORD_SECRET, "x")]).unwrap();
    let err = Email::new(&setup)
        .unwrap()
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert!(!err.retryable);
    assert!(
        err.message
            .contains("gateway.platforms.email.options.imap_host"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn plain_connections_go_only_to_loopback_hosts() {
    let home = tempfile::tempdir().unwrap();
    let table = "options = { address = \"a@b.example\", imap_host = \"imap.b.example\", imap_security = \"plain\", smtp_host = \"smtp.b.example\" }";
    let setup = kit_setup(home.path(), "email", table, &[(PASSWORD_SECRET, "x")]).unwrap();
    let err = Email::new(&setup)
        .unwrap()
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert!(err.message.contains("loopback"), "{}", err.message);
}

#[tokio::test]
async fn a_wrong_password_is_fatal() {
    let home = tempfile::tempdir().unwrap();
    let mock = MailMock::start().await;
    let setup = kit_setup(
        home.path(),
        "email",
        &mock.table(),
        &[(PASSWORD_SECRET, "wrong")],
    )
    .unwrap();
    let err = Email::new(&setup)
        .unwrap()
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "unauthorized");
    assert!(!err.retryable);
}

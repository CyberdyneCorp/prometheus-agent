//! The host side of the adapter contract (spec: platform-adapters):
//! descriptors, degradation, credential locks, deduplication, batching,
//! oversized output, send retries, the webhook listener and the
//! numbered card tier, against the fake adapter.

use std::time::Duration;

use pmt_gateway::messaging::{
    AdapterState, Admission, ChatType, Descriptor, Inbound, Interactive, LengthUnit, MediaKind,
    SendError, SendRequest,
};
use pmt_store::gateway::outbox::OutboundState;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::support::{Did, Harness, Setup, config, eventually, open, said};

fn message(id: &str, text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Dm, "1", "1", text);
    id.clone_into(&mut inbound.message_id);
    inbound
}

#[tokio::test]
async fn one_limit_everywhere_comes_from_the_descriptor() {
    let mut setup = Setup::new(&open("email"));
    setup.descriptors.push(Descriptor {
        splits_long_messages: true,
        ..Descriptor::new("email", 50_000)
    });
    let h = Harness::start(setup).await;
    let reply = "x".repeat(30_000);
    h.dm("email", "1", &format!("say:{reply}")).await;
    said(&h.adapter("email"), &reply).await;
    assert_eq!(h.adapter("email").texts().len(), 1);
    let status = h.gateway.statuses().remove(0);
    assert_eq!(status.descriptor.unwrap().max_message_length, 50_000);
}

#[tokio::test]
async fn a_descriptor_without_a_positive_limit_fails_the_load() {
    let mut setup = Setup::new(&open("matrix"));
    setup.descriptors.push(Descriptor::new("matrix", 0));
    setup.unconnected.push("matrix");
    let h = Harness::start(setup).await;
    let status = h.gateway.statuses().remove(0);
    assert_eq!(status.state, AdapterState::Fatal);
    assert_eq!(status.code.as_deref(), Some("invalid_descriptor"));
}

#[tokio::test]
async fn a_lowered_limit_applies_and_a_raised_one_fails_the_load() {
    let h = Harness::start(Setup::new(&config(
        "discord",
        "allow_all = true\nmax_message_length = 100",
    )))
    .await;
    let status = h.gateway.statuses().remove(0);
    assert_eq!(status.descriptor.unwrap().max_message_length, 100);
    h.dm("discord", "1", &format!("say:{}", "word ".repeat(60)))
        .await;
    eventually("two parts", || h.adapter("discord").texts().len() == 2).await;
    assert!(
        h.adapter("discord")
            .texts()
            .iter()
            .all(|t| t.chars().count() <= 100)
    );
    // First-party maxima are checked by validation; the host checks any
    // other adapter's declared maximum at load.
    let mut setup = Setup::new(&config("test", "max_message_length = 5000"));
    setup.unconnected.push("test");
    let h = Harness::start(setup).await;
    let status = h.gateway.statuses().remove(0);
    assert_eq!(status.state, AdapterState::Fatal);
    assert!(status.message.unwrap().contains("maximum is 4096"));
}

#[tokio::test]
async fn without_editing_the_resolved_card_is_a_new_message() {
    let mut setup = Setup::new(&open("signal"));
    setup.no_edit.push("signal");
    let h = Harness::start(setup).await;
    h.dm("signal", "1", "approval").await;
    said(&h.adapter("signal"), "Approval needed: terminal").await;
    h.dm("signal", "1", "/approve").await;
    said(&h.adapter("signal"), "Approved once by 1 via signal").await;
    assert_eq!(
        h.adapter("signal")
            .edits_tried
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(
        h.adapter("signal")
            .all()
            .iter()
            .all(|d| matches!(d, Did::Send(_)))
    );
}

#[tokio::test]
async fn a_declared_edit_answering_unsupported_is_dropped_for_the_connection() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let adapter = h.adapter("telegram");
    adapter
        .edit_unsupported
        .store(true, std::sync::atomic::Ordering::Relaxed);
    for _ in 0..2 {
        h.dm("telegram", "1", "approval").await;
        said(&adapter, "Approval needed: terminal").await;
        let before = adapter.texts().len();
        h.dm("telegram", "1", "/approve").await;
        eventually("the outcome as a new message", || {
            adapter.texts().len() > before + 1
        })
        .await;
    }
    // The second card's outcome no longer tries the edit.
    assert_eq!(
        adapter
            .edits_tried
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn a_signal_style_numbered_card_is_answered_by_number() {
    let mut setup = Setup::new(&open("signal"));
    setup.descriptors.push(Descriptor {
        interactive: Interactive::Numbered,
        ..Descriptor::new("signal", 8000)
    });
    let h = Harness::start(setup).await;
    h.dm("signal", "1", "approval").await;
    said(
        &h.adapter("signal"),
        "1 Once / 2 Session / 3 Always / 4 Deny / 5 Explain",
    )
    .await;
    let card = h.adapter("signal").all();
    assert!(card.iter().all(|d| match d {
        Did::Send(m) => m.buttons.is_empty(),
        Did::Edit(..) => true,
    }));
    h.dm("signal", "1", "1").await;
    said(&h.adapter("signal"), "Approved once").await;
    said(&h.adapter("signal"), "approved").await;
}

#[tokio::test]
async fn without_buttons_the_card_names_the_slash_commands() {
    let mut setup = Setup::new(&open("webhook"));
    setup.descriptors.push(Descriptor {
        interactive: Interactive::None,
        ..Descriptor::new("webhook", 65_536)
    });
    let h = Harness::start(setup).await;
    h.dm("webhook", "1", "approval").await;
    said(&h.adapter("webhook"), "/approve").await;
    let card = h.adapter("webhook").texts()[0].clone();
    let id = card
        .split("/deny ")
        .nth(1)
        .unwrap()
        .trim_end_matches('.')
        .to_owned();
    h.dm("webhook", "1", &format!("/approve {id} always")).await;
    said(&h.adapter("webhook"), "Approved always").await;
}

#[tokio::test]
async fn an_open_adapter_policy_is_not_trusted() {
    let mut setup = Setup::new(&config("matrix", ""));
    setup.descriptors.push(Descriptor {
        access_policy: Some(pmt_gateway::messaging::AccessPolicy::Open),
        ..Descriptor::new("matrix", 4000)
    });
    let h = Harness::start(setup).await;
    let stranger = Inbound::text(ChatType::Group, "town-square", "stranger", "hello");
    assert_eq!(h.send("matrix", stranger).await, Admission::Unauthorized);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.agents.texts().is_empty());
}

#[tokio::test]
async fn one_credential_in_two_profiles_connects_once() {
    let mut setup = Setup::new(&open("telegram"));
    setup.profiles.push(("work", open("telegram")));
    setup.locks = true;
    setup.credentials = vec![
        (("default", "telegram"), "123:ABC".into()),
        (("work", "telegram"), "123:ABC".into()),
    ];
    // Only one of the two can connect.
    setup.unconnected.push("telegram");
    let h = Harness::start(setup).await;
    eventually("one adapter holds the token", || {
        let states: Vec<AdapterState> = h.gateway.statuses().iter().map(|s| s.state).collect();
        states.contains(&AdapterState::Fatal)
            && states
                .iter()
                .any(|s| matches!(s, AdapterState::Connected | AdapterState::Degraded))
    })
    .await;
    let statuses = h.gateway.statuses();
    let fatal = statuses
        .iter()
        .find(|s| s.state == AdapterState::Fatal)
        .unwrap();
    let holder = statuses
        .iter()
        .find(|s| s.state != AdapterState::Fatal)
        .unwrap();
    assert_eq!(fatal.code.as_deref(), Some("telegram_lock"));
    let message = fatal.message.clone().unwrap();
    assert!(
        message.contains(&format!("profile '{}'", holder.profile)),
        "{message}"
    );
    assert!(message.contains(&format!("prometheus -p {} gateway stop", holder.profile)));
}

#[tokio::test]
async fn a_redelivered_event_is_a_duplicate_and_starts_nothing() {
    let h = Harness::start(Setup::new(&open("slack"))).await;
    assert_eq!(
        h.send("slack", message("ev1", "hi")).await,
        Admission::Accepted
    );
    said(&h.adapter("slack"), "echo: hi").await;
    assert_eq!(
        h.send("slack", message("ev1", "hi")).await,
        Admission::Duplicate
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.agents.texts(), ["hi"]);
}

#[tokio::test]
async fn a_split_paste_is_batched_into_one_turn() {
    let table = "allow_all = true\n[gateway.platforms.telegram.batching]\ndelay_ms = 300";
    let h = Harness::start(Setup::new(&config("telegram", table))).await;
    for (i, part) in ["first part", "second part", "third part"]
        .iter()
        .enumerate()
    {
        h.send("telegram", message(&format!("m{i}"), part)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    said(&h.adapter("telegram"), "echo: first part").await;
    assert_eq!(h.agents.texts(), ["first part\nsecond part\nthird part"]);
}

#[tokio::test]
async fn long_scheduled_output_is_truncated_with_the_full_text_attached() {
    let mut setup = Setup::new(&config(
        "telegram",
        "allow_all = true\nhome_channel = { chat_id = \"-100\" }",
    ));
    setup.descriptors.push(Descriptor {
        length_unit: LengthUnit::Utf16,
        media_out: [MediaKind::Document].into_iter().collect(),
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    let output = "line of cron output\n".repeat(6000);
    assert!(output.len() >= 120_000);
    let request = SendRequest {
        target: "telegram".into(),
        text: output.clone(),
        attachments: Vec::new(),
        warning: false,
    };
    h.gateway.send("default", &request).await.unwrap();
    let adapter = h.adapter("telegram");
    eventually("two messages", || adapter.texts().len() == 2).await;
    let sent: Vec<_> = adapter
        .all()
        .into_iter()
        .filter_map(|d| match d {
            Did::Send(m) => Some(m),
            Did::Edit(..) => None,
        })
        .collect();
    assert!(sent[0].text.encode_utf16().count() <= 4096);
    assert!(
        sent[1]
            .text
            .ends_with("... [truncated, full output attached]")
    );
    let document = &sent[1].attachments[0];
    assert!(document.as_document);
    assert_eq!(std::fs::read_to_string(&document.path).unwrap(), output);
}

#[tokio::test]
async fn a_long_flood_wait_goes_back_to_the_outbox_at_once() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.adapter("telegram")
        .failures
        .lock()
        .unwrap()
        .push_back(SendError::RateLimited {
            retry_after: Some(Duration::from_mins(5)),
        });
    let started = std::time::Instant::now();
    h.dm("telegram", "1", "hello").await;
    let store = h.store();
    eventually("the entry waits", || {
        store
            .outbox_entries(Some(OutboundState::Queued), 10)
            .unwrap()
            .iter()
            .any(|e| e.attempts == 1)
    })
    .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let entry = store
        .outbox_entries(Some(OutboundState::Queued), 10)
        .unwrap()
        .remove(0);
    let wait = entry.next_attempt_at.duration_since(jiff::Timestamp::now());
    assert!(wait.as_secs() >= 290, "{wait:?}");
}

#[tokio::test]
async fn refused_markup_is_resent_once_as_plain_text() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.adapter("telegram")
        .failures
        .lock()
        .unwrap()
        .push_back(SendError::Formatting("can't parse entities".into()));
    h.dm("telegram", "1", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    let did = h.adapter("telegram").all();
    let Did::Send(sent) = &did[0] else {
        panic!("{did:?}")
    };
    assert!(sent.plain);
}

#[tokio::test]
async fn a_slow_typing_call_never_stalls_the_cadence() {
    let mut setup = Setup::new(&open("telegram"));
    setup.descriptors.push(Descriptor {
        supports_typing: true,
        ..Descriptor::new("telegram", 4096)
    });
    let h = Harness::start(setup).await;
    *h.adapter("telegram").typing_delay.lock().unwrap() = Duration::from_secs(5);
    h.dm("telegram", "1", "slow:5000").await;
    said(&h.adapter("telegram"), "slow done").await;
    let calls = h.adapter("telegram").typing.lock().unwrap().clone();
    assert!(calls.len() >= 3, "{} typing calls", calls.len());
    for pair in calls.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(gap < Duration::from_millis(2600), "{gap:?}");
    }
}

/// One HTTP/1.1 request on the webhook listener; the status code.
async fn post(addr: std::net::SocketAddr, path: &str, secret: Option<&str>, body: &str) -> u16 {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let header = secret.map_or_else(String::new, |s| format!("x-secret: {s}\r\n"));
    let request = format!(
        "POST {path} HTTP/1.1\r\nhost: localhost\r\n{header}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap()
}

#[tokio::test]
async fn two_profiles_share_one_webhook_listener_and_unsigned_requests_get_401() {
    let mut setup = Setup::new(&open("webhook"));
    setup.profiles.push(("work", open("webhook")));
    setup.descriptors.push(Descriptor {
        binds_http: true,
        ..Descriptor::new("webhook", 65_536)
    });
    let h = Harness::start(setup).await;
    for profile in ["default", "work"] {
        *h.adapter_of(profile, "webhook").http_secret.lock().unwrap() = Some("s3cret".into());
    }
    let addr = h.gateway.http_addr().expect("the listener runs");
    assert_eq!(
        post(addr, "/p/default/webhook/", Some("wrong"), "x").await,
        401
    );
    assert_eq!(post(addr, "/p/default/webhook/", None, "x").await, 401);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.agents.texts().is_empty());
    assert_eq!(
        post(addr, "/p/default/webhook/", Some("s3cret"), "deploy done").await,
        200
    );
    assert_eq!(
        post(addr, "/p/work/webhook/", Some("s3cret"), "build red").await,
        200
    );
    assert_eq!(
        post(addr, "/p/nobody/webhook/", Some("s3cret"), "x").await,
        404
    );
    said(&h.adapter_of("default", "webhook"), "echo: deploy done").await;
    said(&h.adapter_of("work", "webhook"), "echo: build red").await;
}

#[tokio::test]
async fn attached_clients_see_adapter_state_changes() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    let session = h.sessions()[0].to_string();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let desktop = pmt_gateway::hub::Peer {
        tx,
        minor: h.server.protocol().minor,
        advertised: std::sync::Arc::default(),
        server_requests: false,
        client_id: "desktop-901".into(),
        role: pmt_rpc::contracts::AttachRole::Observer,
    };
    h.server.hub().attach(901_000, &session, desktop, None);
    let link = h.adapter("telegram").link.lock().unwrap().clone().unwrap();
    link.report(
        AdapterState::Retrying,
        Some(pmt_gateway::messaging::AdapterError {
            retryable: true,
            code: "telegram_unreachable".into(),
            message: "connection reset".into(),
        }),
    );
    let frame = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = rx.recv().await.expect("a frame");
            let text = message.into_text().unwrap().to_string();
            if text.contains("\"adapter.state\"") {
                return text;
            }
        }
    })
    .await
    .expect("an adapter.state event");
    assert!(frame.contains("\"state\":\"retrying\""), "{frame}");
    assert!(
        frame.contains("\"code\":\"telegram_unreachable\""),
        "{frame}"
    );
}

//! Home channels and messages sent without a turn (spec: messaging-
//! gateway, Home channel and /sethome; agent-interop, One-shot send
//! command; `send_message` tool).

use pmt_gateway::messaging::target::Resolved;
use pmt_gateway::messaging::{ChatType, Inbound, SendRequest};

use crate::support::{Did, Harness, Setup, config, eventually, said};

fn request(target: &str, text: &str) -> SendRequest {
    let (text, attachments) = pmt_gateway::messaging::target::media(text);
    SendRequest {
        target: target.into(),
        text,
        attachments,
    }
}

#[tokio::test]
async fn sethome_makes_scheduled_output_go_to_that_chat_and_topic() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\ngroup_admins = [\"boss\"]",
    )))
    .await;
    let mut inbound = Inbound::text(ChatType::Thread, "-100123", "boss", "/sethome");
    inbound.thread_id = Some("7".into());
    h.send("telegram", inbound).await;
    said(&h.adapter("telegram"), "Home channel set to -100123:7").await;
    let config =
        std::fs::read_to_string(h.home.path().join("profiles/default/config.toml")).unwrap();
    assert!(config.contains("-100123"), "{config}");
    // A job with deliver = "telegram" completes.
    let entry = h
        .gateway
        .send("default", &request("telegram", "nightly report"))
        .await
        .unwrap();
    assert_eq!(entry.chat_id, "-100123");
    said(&h.adapter("telegram"), "nightly report").await;
    let sent = h
        .adapter("telegram")
        .all()
        .into_iter()
        .find_map(|d| match d {
            Did::Send(m) if m.text == "nightly report" => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(sent.chat_id, "-100123");
    assert_eq!(sent.thread_id.as_deref(), Some("7"));
    assert!(h.agents.texts().is_empty(), "no agent turn");
}

#[tokio::test]
async fn sethome_outside_a_thread_clears_an_old_thread() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\n[gateway.platforms.telegram.home_channel]\nchat_id = \"1\"\nthread_id = \"9\"",
    )))
    .await;
    h.send(
        "telegram",
        Inbound::text(ChatType::Group, "-200", "u", "/set-home"),
    )
    .await;
    said(&h.adapter("telegram"), "Home channel set to -200").await;
    let resolved = h
        .gateway
        .resolve_target("default", "telegram")
        .await
        .unwrap();
    assert_eq!(
        resolved,
        Resolved {
            platform: "telegram".into(),
            chat_id: "-200".into(),
            thread_id: None,
        }
    );
}

#[tokio::test]
async fn a_delivery_without_a_home_channel_fails_naming_sethome() {
    let mut text = config("telegram", "allow_all = true");
    text.push_str("\n[gateway.platforms.slack]\nenabled = true\nallow_all = true\n");
    let h = Harness::start(Setup::new(&text)).await;
    let err = h
        .gateway
        .send("default", &request("slack", "hi"))
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "no home channel for slack; run /sethome in the target chat"
    );
    assert_eq!(err.exit_code(), 66);
    let err = h
        .gateway
        .send("default", &request("signal:1", "hi"))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 78, "{err}");
}

#[tokio::test]
async fn media_directives_travel_as_attachments() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    h.gateway
        .send(
            "default",
            &request(
                "telegram:-1001234567890",
                "chart\nMEDIA:/data/chart.png\n[[as_document]]",
            ),
        )
        .await
        .unwrap();
    said(&h.adapter("telegram"), "chart").await;
    let sent = h
        .adapter("telegram")
        .all()
        .into_iter()
        .find_map(|d| match d {
            Did::Send(m) => Some(m),
            Did::Edit(..) => None,
        })
        .unwrap();
    assert_eq!(sent.attachments.len(), 1);
    assert!(sent.attachments[0].as_document);
    assert_eq!(sent.attachments[0].path, "/data/chart.png");
}

#[tokio::test]
async fn the_tool_may_reach_only_home_allowed_or_known_chats() {
    let h = Harness::start(Setup::new(&config(
        "telegram",
        "allow_all = true\nallow_chats = [\"-300\"]\n[gateway.platforms.telegram.home_channel]\nchat_id = \"-100\"",
    )))
    .await;
    h.dm("telegram", "555", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    let resolve = |t: &str| {
        let gateway = h.gateway.clone();
        let t = t.to_owned();
        async move { gateway.resolve_target("default", &t).await.unwrap() }
    };
    for allowed in ["telegram", "telegram:-300", "telegram:555"] {
        let to = resolve(allowed).await;
        h.gateway
            .authorize_destination("default", &to)
            .await
            .unwrap_or_else(|e| panic!("{allowed}: {e}"));
    }
    let unknown = resolve("telegram:999").await;
    let err = h
        .gateway
        .authorize_destination("default", &unknown)
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "target telegram:999 is not an authorized destination"
    );
}

#[tokio::test]
async fn a_sent_message_is_mirrored_into_the_chats_session() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    h.dm("telegram", "555", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    let session = h.sessions()[0];
    let to = h
        .gateway
        .resolve_target("default", "telegram:555")
        .await
        .unwrap();
    h.gateway
        .mirror("default", &to, "reminder: standup", Some("ses_other"));
    eventually("the mirrored message", || {
        h.store()
            .messages(session, pmt_store::Visibility::All)
            .unwrap()
            .iter()
            .any(|m| m.content.as_deref() == Some("reminder: standup"))
    })
    .await;
}

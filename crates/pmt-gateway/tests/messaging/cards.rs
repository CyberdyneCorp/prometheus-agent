//! The card model on every tier and adapter access-policy delegation
//! (spec: platform-adapters, Interactive approval cards on every platform;
//! Adapter access-policy delegation), against the fake adapter.

use std::sync::atomic::Ordering;

use pmt_gateway::messaging::{
    AccessPolicy, AdapterError, Admission, ChatType, Descriptor, Inbound, Interactive,
    SenderVerdict,
};

use crate::support::{Did, Harness, Setup, config, eventually, open, said};

/// The first message carrying buttons.
fn card_with_buttons(h: &Harness, platform: &str) -> pmt_gateway::messaging::Outgoing {
    h.adapter(platform)
        .all()
        .into_iter()
        .find_map(|d| match d {
            Did::Send(m) if !m.buttons.is_empty() => Some(m),
            _ => None,
        })
        .expect("a card with buttons")
}

fn press(data: &str) -> Inbound {
    let mut press = Inbound::text(ChatType::Dm, "1", "1", "");
    press.callback = Some(data.to_owned());
    press
}

fn tiered(platform: &str, interactive: Interactive) -> Setup {
    let mut setup = Setup::new(&open(platform));
    setup.descriptors.push(Descriptor {
        interactive,
        supports_edit: true,
        ..Descriptor::new(platform, 4000)
    });
    setup
}

#[tokio::test]
async fn every_tier_shows_the_directory_and_the_expiry() {
    for (platform, tier) in [
        ("telegram", Interactive::Native),
        ("signal", Interactive::Numbered),
        ("webhook", Interactive::None),
    ] {
        let h = Harness::start(tiered(platform, tier)).await;
        *h.agents.cwd.lock().unwrap() = Some("/srv/app".into());
        h.dm(platform, "1", "approval").await;
        said(&h.adapter(platform), "Approval needed: terminal").await;
        let card = h.adapter(platform).texts()[0].clone();
        assert!(card.contains("Action: rm -rf build"), "{tier:?}: {card}");
        assert!(card.contains("Directory: /srv/app"), "{tier:?}: {card}");
        // The fake turn waits 30 s for an answer.
        assert!(card.contains("Expires at "), "{tier:?}: {card}");
        assert!(
            card.contains("(in 30 s); unanswered, it is denied"),
            "{tier:?}: {card}"
        );
    }
}

#[tokio::test]
async fn a_clarify_choice_is_answered_by_a_button_press() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    let adapter = h.adapter("telegram");
    h.dm("telegram", "1", "clarify").await;
    said(&adapter, "Which colour?").await;
    let card = card_with_buttons(&h, "telegram");
    let labels: Vec<&str> = card.buttons.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["red", "green", "blue"]);
    assert!(card.text.contains("Expires at "), "{}", card.text);
    let green = card.buttons[1].data.clone();
    assert!(green.starts_with("clar:"), "{green}");
    // A tampered press decides nothing.
    h.send("telegram", press(&green.replace(":0.2:", ":0.3:")))
        .await;
    h.send("telegram", press(&green)).await;
    said(&adapter, "Why?").await;
    // The answered question's buttons are spent.
    h.send("telegram", press(&card.buttons[0].data)).await;
    h.dm("telegram", "1", "it is calm").await;
    said(
        &adapter,
        r#"clarified: [{"choices":["green"],"status":"answered"},{"status":"answered","text":"it is calm"}]"#,
    )
    .await;
}

#[tokio::test]
async fn a_numbered_clarify_has_no_buttons_and_takes_the_number() {
    let h = Harness::start(tiered("signal", Interactive::Numbered)).await;
    let adapter = h.adapter("signal");
    h.dm("signal", "1", "clarify").await;
    said(&adapter, "3. blue").await;
    assert!(adapter.all().iter().all(|d| match d {
        Did::Send(m) => m.buttons.is_empty(),
        Did::Edit(..) => true,
    }));
    h.dm("signal", "1", "3").await;
    said(&adapter, "Why?").await;
    h.dm("signal", "1", "sky").await;
    said(&adapter, r#"{"choices":["blue"],"status":"answered"}"#).await;
}

#[tokio::test]
async fn only_an_allow_from_an_adapter_allowlist_admits() {
    let mut setup = Setup::new(&config("matrix", ""));
    setup.descriptors.push(Descriptor {
        access_policy: Some(AccessPolicy::Allowlist),
        ..Descriptor::new("matrix", 4000)
    });
    let h = Harness::start(setup).await;
    let adapter = h.adapter("matrix");
    {
        let mut verdicts = adapter.verdicts.lock().unwrap();
        verdicts.insert("friend".into(), Ok(SenderVerdict::Allow));
        verdicts.insert("foe".into(), Ok(SenderVerdict::Deny));
        verdicts.insert(
            "broken".into(),
            Err(AdapterError {
                retryable: true,
                code: "matrix_unreachable".into(),
                message: "the homeserver did not answer".into(),
            }),
        );
    }
    let table = [
        ("friend", Admission::Accepted),
        ("foe", Admission::Unauthorized),
        ("broken", Admission::Unauthorized),
        ("stranger", Admission::Unauthorized),
    ];
    for (n, (sender, expected)) in table.into_iter().enumerate() {
        let mut message = Inbound::text(ChatType::Group, "room", sender, "hello");
        message.message_id = format!("m{n}");
        assert_eq!(h.send("matrix", message).await, expected, "{sender}");
    }
    eventually("the allowed sender's turn", || h.agents.texts().len() == 1).await;
    assert_eq!(adapter.sender_checks.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn a_gateway_rule_admits_before_the_adapter_is_asked() {
    let mut setup = Setup::new(&config("matrix", "allow_users = [\"friend\"]"));
    setup.descriptors.push(Descriptor {
        access_policy: Some(AccessPolicy::Allowlist),
        ..Descriptor::new("matrix", 4000)
    });
    let h = Harness::start(setup).await;
    let message = Inbound::text(ChatType::Group, "room", "friend", "hello");
    assert_eq!(h.send("matrix", message).await, Admission::Accepted);
    assert_eq!(h.adapter("matrix").sender_checks.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn an_open_adapter_policy_is_never_asked() {
    let mut setup = Setup::new(&config("matrix", ""));
    setup.descriptors.push(Descriptor {
        access_policy: Some(AccessPolicy::Open),
        ..Descriptor::new("matrix", 4000)
    });
    let h = Harness::start(setup).await;
    let adapter = h.adapter("matrix");
    adapter
        .verdicts
        .lock()
        .unwrap()
        .insert("stranger".into(), Ok(SenderVerdict::Allow));
    let message = Inbound::text(ChatType::Group, "room", "stranger", "hello");
    assert_eq!(h.send("matrix", message).await, Admission::Unauthorized);
    assert_eq!(adapter.sender_checks.load(Ordering::Relaxed), 0);
}

/// Spec: messaging-gateway, Webhook route delivery and safe default
/// toolset: where nobody can answer (a webhook route), clarify is answered
/// as unavailable at once and no question is sent.
#[tokio::test]
async fn clarify_is_unavailable_where_nobody_can_answer() {
    let mut setup = Setup::new(&open("webhook"));
    setup.descriptors.push(pmt_gateway::messaging::Descriptor {
        interactive: pmt_gateway::messaging::Interactive::None,
        ..pmt_gateway::messaging::Descriptor::new("webhook", 4096)
    });
    let h = Harness::start(setup).await;
    h.dm("webhook", "hook", "clarify").await;
    said(&h.adapter("webhook"), "clarified: null").await;
    let texts = h.adapter("webhook").texts();
    assert!(
        !texts.iter().any(|t| t.contains("Which colour?")),
        "{texts:?}"
    );
}

/// Spec: messaging-gateway, Webhook route delivery and safe default
/// toolset: MCP notifications reach a webhook route only while
/// `gateway.mcp_events.enabled` is on (the gateway vouches for them).
#[tokio::test]
async fn mcp_events_reach_the_webhook_route_only_when_enabled() {
    let payload = serde_json::json!({"uri": "file:///notes.md"});
    let h = Harness::start(Setup::new(&open("webhook"))).await;
    let refused = h
        .gateway
        .mcp_event("default", "mcp", "resource.updated", "n1", &payload)
        .await
        .unwrap_err();
    assert!(refused.message().contains("gateway.mcp_events.enabled"));
    let on = format!(
        "{}\n[gateway.mcp_events]\nenabled = true\n",
        open("webhook")
    );
    let h = Harness::start(Setup::new(&on)).await;
    let answer = h
        .gateway
        .mcp_event("default", "mcp", "resource.updated", "n1", &payload)
        .await
        .unwrap();
    assert_eq!(answer.status, 200);
    said(&h.adapter("webhook"), "file:///notes.md").await;
}

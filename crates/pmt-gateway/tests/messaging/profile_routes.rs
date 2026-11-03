//! Shared-credential profile routes (spec: messaging-gateway,
//! Shared-credential profile routes): one bot, several communities.

use pmt_gateway::messaging::{ChatType, Inbound};

use crate::support::{Harness, Setup, config, said};

fn routed_setup(accept: &str) -> Setup {
    let mut setup = Setup::new(&format!(
        "{}\n[[gateway.profile_routes]]\nname = \"acme\"\nplatform = \"telegram\"\nguild_id = \"123\"\nprofile = \"acme\"\n",
        config("telegram", "allow_all = true")
    ));
    // The target first: the router's config is checked against it.
    setup.profiles.insert(
        0,
        (
            "acme",
            format!("[gateway]\naccept_routes_from = [{accept}]\n"),
        ),
    );
    setup
}

fn in_guild(guild: &str, chat: &str, text: &str) -> Inbound {
    let mut inbound = Inbound::text(ChatType::Group, chat, "u1", text);
    inbound.scope_id = Some(guild.to_owned());
    inbound.addressing.mentioned = true;
    inbound
}

/// Spec scenario: One Discord bot, two communities (on the fake adapter).
#[tokio::test]
async fn a_routed_event_runs_in_the_target_profile_and_the_receiving_bot_replies() {
    let h = Harness::start(routed_setup("\"default\"")).await;
    h.send("telegram", in_guild("123", "c1", "hello acme"))
        .await;
    said(&h.adapter("telegram"), "echo: hello acme").await;
    let acme = h.sessions_of("acme");
    assert_eq!(acme.len(), 1, "the session belongs to acme");
    assert!(h.sessions_of("default").is_empty());
    let routes = h
        .agents
        .store_of("acme")
        .thread_routes("telegram", "c1")
        .unwrap();
    assert!(routes.is_empty(), "a channel, not a thread");
    let route = h
        .agents
        .store_of("acme")
        .routed_session(acme[0])
        .unwrap()
        .unwrap();
    assert!(
        route.route_key.starts_with("acme:telegram@123:"),
        "{}",
        route.route_key
    );
    h.send("telegram", in_guild("999", "c2", "hello default"))
        .await;
    said(&h.adapter("telegram"), "echo: hello default").await;
    assert_eq!(h.sessions_of("default").len(), 1, "unmatched events stay");
}

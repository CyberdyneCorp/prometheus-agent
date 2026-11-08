//! `external_process` providers (spec: model-providers, Plugin OAuth and
//! external-process providers) against the fake ACP agent CLI
//! `pmt-acp-fixture`.

use pmt_providers::StreamEvent;
use pmt_providers::resolve::{Target, Trace};

use crate::support::{Env, caller, request};

fn fixture() -> String {
    env!("CARGO_BIN_EXE_pmt-acp-fixture").replace('\\', "/")
}

fn config(command: &str) -> String {
    format!("[providers.agent-cli]\nauth = \"external_process\"\ncommand = \"{command}\"\n")
}

#[tokio::test]
async fn an_agent_cli_answers_over_acp_with_its_own_sign_in() {
    let env = Env::new(&config(&fixture()), &[]);
    let route = env.route("agent-cli", "default").await;
    assert!(route.base_url.starts_with("acp:///"), "{}", route.base_url);
    assert_eq!(route.billing_source(), "subscription:agent-cli/external");
    let mut stream = caller()
        .send(&route, &request("what is two plus two"), None)
        .await
        .unwrap();
    let (mut text, mut thought) = (String::new(), String::new());
    for event in stream.collect().await {
        match event {
            StreamEvent::TextDelta { text: t } => text.push_str(&t),
            StreamEvent::ReasoningDelta { text: t } => thought.push_str(&t),
            _ => {}
        }
    }
    assert_eq!(text, "agent saw: User: what is two plus two");
    // The CLI asked for a permission Prometheus cannot grant on its behalf.
    assert_eq!(thought, "permission cancelled");
    assert!(stream.failure().is_none());
}

/// Scenario: Missing external CLI.
#[tokio::test]
async fn a_missing_agent_cli_fails_the_route_naming_it() {
    let env = Env::new(&config("pmt-no-such-agent-cli"), &[]);
    let target = Target {
        provider: "agent-cli".to_owned(),
        ..Target::default()
    };
    let err = env
        .resolver()
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.external_cli_missing");
    assert!(
        err.message().contains("'pmt-no-such-agent-cli'"),
        "{}",
        err.message()
    );
}

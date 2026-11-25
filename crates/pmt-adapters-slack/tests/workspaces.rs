//! One Slack app serving several workspaces (spec: platform-adapters,
//! Slack multiple workspaces): tokens from `options.bot_token_secrets` and
//! OAuth installs (`SLACK_BOT_TOKEN_<team>`), de-duplicated, checked with
//! `auth.test`, events and replies routed by team, and a refused token
//! degrading only its workspace.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_slack::mock::{APP_TOKEN, BOT_TOKEN, CHANNEL, SlackMock, TEAM};
use pmt_adapters_slack::{APP_TOKEN_SECRET, Slack, TOKEN_SECRET};
use pmt_gateway::messaging::adapter::PARTIAL_OUTAGE;
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, Outgoing,
};

const T2: &str = "T0TWO";
const T2_TOKEN: &str = "xoxb-two";
const T2_BOT: &str = "U0BOT2";
const T2_CHANNEL: &str = "C0TWO";

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Inbound>>,
    reports: Mutex<Vec<(AdapterState, Option<AdapterError>)>>,
}

impl AdapterLink for Recorder {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, state: AdapterState, error: Option<AdapterError>) {
        self.reports.lock().unwrap().push((state, error));
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn adapter(home: &std::path::Path, mock: &SlackMock, table: &str, extra: &[(&str, &str)]) -> Slack {
    let mut secrets = vec![(TOKEN_SECRET, BOT_TOKEN), (APP_TOKEN_SECRET, APP_TOKEN)];
    secrets.extend_from_slice(extra);
    let setup = kit_setup(home, "slack", table, &secrets).unwrap();
    Slack::new(&setup, mock.endpoints()).unwrap()
}

async fn two_workspaces() -> Arc<SlackMock> {
    let mock = SlackMock::start().await;
    mock.add_workspace(T2_TOKEN, T2, T2_BOT);
    mock.channel_in(T2_CHANNEL, T2);
    mock
}

/// Scenario: Reply goes to the right workspace.
#[tokio::test]
async fn a_message_from_the_second_workspace_is_answered_with_its_token() {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mock = two_workspaces().await;
    let slack = adapter(
        home.path(),
        &mock,
        "options = { bot_token_secrets = [\"SLACK_TWO\"] }",
        &[("SLACK_TWO", T2_TOKEN)],
    );
    let link = Arc::new(Recorder::default());
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    mock.inject_in(T2, T2_CHANNEL, "U0HUMAN", &format!("<@{T2_BOT}> status?"));
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.scope_id.as_deref(), Some(T2));
    assert!(event.addressing.mentioned, "the bot of T2 is recognised");
    let sent = slack
        .send(&Outgoing::text(T2_CHANNEL, None, "all green"))
        .await
        .unwrap();
    let ts = sent.message_id.unwrap();
    assert_eq!(mock.posted_by(&ts).as_deref(), Some(T2));
    // The primary workspace's channels still go out with its own token.
    let sent = slack
        .send(&Outgoing::text(CHANNEL, None, "hello ops"))
        .await
        .unwrap();
    assert_eq!(
        mock.posted_by(&sent.message_id.unwrap()).as_deref(),
        Some(TEAM)
    );
    slack.disconnect().await;
}

#[tokio::test]
async fn a_channel_never_seen_is_found_in_its_workspace_after_a_restart() {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mock = two_workspaces().await;
    // An OAuth install keyed by its team id.
    let slack = adapter(
        home.path(),
        &mock,
        "",
        &[(&format!("SLACK_BOT_TOKEN_{T2}"), T2_TOKEN)],
    );
    slack
        .connect(Arc::new(Recorder::default()) as _)
        .await
        .unwrap();
    let sent = slack
        .send(&Outgoing::text(T2_CHANNEL, None, "scheduled digest"))
        .await
        .unwrap();
    assert_eq!(
        mock.posted_by(&sent.message_id.unwrap()).as_deref(),
        Some(T2)
    );
    slack.disconnect().await;
}

#[tokio::test]
async fn the_same_token_under_two_names_is_one_workspace() {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mock = SlackMock::start().await;
    let slack = adapter(
        home.path(),
        &mock,
        "options = { bot_token_secrets = [\"SLACK_AGAIN\"] }",
        &[("SLACK_AGAIN", BOT_TOKEN)],
    );
    let link = Arc::new(Recorder::default());
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    assert!(link.reports.lock().unwrap().is_empty());
    slack.disconnect().await;
}

#[tokio::test]
async fn a_refused_token_degrades_only_its_workspace_naming_the_team() {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mock = SlackMock::start().await;
    let slack = adapter(
        home.path(),
        &mock,
        "",
        &[("SLACK_BOT_TOKEN_T0GONE", "xoxb-revoked")],
    );
    let link = Arc::new(Recorder::default());
    slack.connect(Arc::clone(&link) as _).await.unwrap();
    let reports = link.reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    let (state, error) = &reports[0];
    assert_eq!(*state, AdapterState::Degraded);
    let error = error.clone().unwrap();
    assert_eq!(error.code, PARTIAL_OUTAGE);
    assert!(error.message.contains("T0GONE"), "{}", error.message);
    // The served workspace still works.
    slack
        .send(&Outgoing::text(CHANNEL, None, "still here"))
        .await
        .unwrap();
    slack.disconnect().await;
}

#[tokio::test]
async fn no_token_passing_is_unauthorized() {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mock = SlackMock::start().await;
    let setup = kit_setup(
        home.path(),
        "slack",
        "",
        &[(TOKEN_SECRET, "xoxb-wrong"), (APP_TOKEN_SECRET, APP_TOKEN)],
    )
    .unwrap();
    let slack = Slack::new(&setup, mock.endpoints()).unwrap();
    let err = slack
        .connect(Arc::new(Recorder::default()) as _)
        .await
        .unwrap_err();
    assert_eq!(err.code, "unauthorized");
    assert!(!err.retryable);
}

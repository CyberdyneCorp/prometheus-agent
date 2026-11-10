//! The Discord adapter against a mock REST API and Gateway (spec:
//! platform-adapters): the conformance kit, resume after a dropped
//! connection, the Message Content intent, media in and out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_discord::mock::{self, DiscordMock, TOKEN};
use pmt_adapters_discord::{Discord, TOKEN_SECRET};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::SecretString;
use pmt_gateway::messaging::conformance::{self, MockPlatform as _};
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterSetup, AdapterState, Admission,
    Inbound, MediaKind, Outgoing,
};
use pmt_store::gateway::outbox::OutboundAttachment;

fn profile(home: &std::path::Path, token: bool) -> AdapterSetup {
    profile_with(home, token, "")
}

fn profile_with(home: &std::path::Path, token: bool, extra: &str) -> AdapterSetup {
    let layout = Home::open(home).unwrap();
    let dir = layout
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!("[gateway.platforms.discord]\nenabled = true\n{extra}"),
    )
    .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    if token {
        scope
            .secrets()
            .store()
            .set(TOKEN_SECRET, &SecretString::new(TOKEN))
            .unwrap();
    }
    let config = scope.config().gateway.platform("discord");
    AdapterSetup {
        scope,
        platform: "discord".into(),
        config,
    }
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Inbound>>,
    states: Mutex<Vec<(AdapterState, Option<AdapterError>)>>,
}

impl AdapterLink for Recorder {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, state: AdapterState, error: Option<AdapterError>) {
        self.states.lock().unwrap().push((state, error));
    }
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
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let report = conformance::run(Arc::new(adapter), Arc::clone(&mock) as _).await;
    println!("{}", report.render());
    assert!(report.passed(), "{}", report.render());
    assert_eq!(mock.handshakes(), (1, 1), "one identify, then a resume");
}

#[tokio::test]
async fn a_lost_connection_resumes_and_replays_what_was_missed() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    mock.inject("before");
    eventually(|| link.events.lock().unwrap().len() == 1).await;
    mock.drop_connection();
    eventually(|| {
        link.states
            .lock()
            .unwrap()
            .iter()
            .any(|(s, _)| *s == AdapterState::Retrying)
    })
    .await;
    for text in ["one", "two", "three"] {
        mock.inject(text);
    }
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    eventually(|| link.events.lock().unwrap().len() == 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let texts: Vec<String> = link
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.text.clone())
        .collect();
    assert_eq!(texts, ["before", "one", "two", "three"]);
}

#[tokio::test]
async fn without_the_message_content_intent_the_connection_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    mock.refuse_identify(4014);
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let err = adapter
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert!(!err.retryable);
    assert!(
        err.message.contains("enable the Message Content intent"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn a_missing_token_is_a_fatal_missing_secret() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), false), mock.endpoints()).unwrap();
    let err = adapter
        .connect(Arc::new(Recorder::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, "missing_secret");
    assert!(err.message.contains("DISCORD_BOT_TOKEN"));
}

#[tokio::test]
async fn files_download_from_the_cdn_and_upload_as_attachments() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    let url = mock.add_file("report.pdf", vec![9; 10]);
    mock.inject_file(&url, "report.pdf", "application/pdf");
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.attachments[0].kind, MediaKind::Document);
    assert_eq!(event.attachments[0].name.as_deref(), Some("report.pdf"));
    let mut stream = adapter.open_media(&event.attachments[0]).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, vec![9; 10]);
    let file = home.path().join("chart.png");
    std::fs::write(&file, b"PNG").unwrap();
    let mut message = Outgoing::text(mock::DM, None, "the chart");
    message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    adapter.send(&message).await.unwrap();
    assert_eq!(mock.uploads(), 1);
    assert_eq!(mock.sent().last().unwrap().text, "the chart");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_proxy_carries_rest_and_gateway_traffic() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let (proxy, log) = pmt_gateway::messaging::mock_server::proxy().await.unwrap();
    let setup = profile_with(
        home.path(),
        true,
        &format!("proxy_url = \"http://{proxy}\"\n"),
    );
    let adapter = Discord::new(&setup, mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    adapter
        .send(&Outgoing::text("100", None, "through the proxy"))
        .await
        .unwrap();
    let seen = log.lock().unwrap().clone();
    assert!(
        seen.iter().any(|d| d.starts_with("CONNECT ")),
        "the Gateway WebSocket tunnels through the proxy: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|d| d.starts_with("POST http://") || d.starts_with("GET http://")),
        "REST calls go through the proxy: {seen:?}"
    );
    adapter.disconnect().await;
}

/// Runs [`ambient_child`] in a child process whose environment names
/// `proxy` in every proxy variable; whether it succeeded.
fn run_ambient_child(proxy: std::net::SocketAddr, trust_env: bool) -> bool {
    let url = format!("http://{proxy}");
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ambient_child", "--ignored", "--test-threads=1"])
        .env("HTTP_PROXY", &url)
        .env("HTTPS_PROXY", &url)
        .env("ALL_PROXY", &url)
        .env(
            "PMT_TEST_DISCORD_TRUST_ENV",
            if trust_env { "1" } else { "0" },
        )
        .status()
        .unwrap()
        .success()
}

#[tokio::test(flavor = "multi_thread")]
async fn ambient_proxy_variables_are_ignored_unless_the_environment_is_trusted() {
    let (proxy, log) = pmt_gateway::messaging::mock_server::proxy().await.unwrap();
    let ignored = pmt_core::spawn::blocking(move || run_ambient_child(proxy, false));
    assert!(ignored.await.unwrap());
    assert!(
        log.lock().unwrap().is_empty(),
        "HTTPS_PROXY was used: {:?}",
        log.lock().unwrap()
    );
    let trusted = pmt_core::spawn::blocking(move || run_ambient_child(proxy, true));
    assert!(trusted.await.unwrap());
    assert!(
        !log.lock().unwrap().is_empty(),
        "with gateway.trust_env the environment's proxy carries the traffic"
    );
}

/// The child of the test above: connects and sends once with the
/// inherited proxy environment.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "run by ambient_proxy_variables_are_ignored_unless_the_environment_is_trusted"]
async fn ambient_child() {
    let trust = std::env::var("PMT_TEST_DISCORD_TRUST_ENV").as_deref() == Ok("1");
    let home = tempfile::tempdir().unwrap();
    let setup = profile_with(home.path(), true, "");
    let setup = if trust {
        let file = setup.scope.dir().join("config.toml");
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("[gateway]\ntrust_env = true\n\n{text}")).unwrap();
        profile_reopen(home.path())
    } else {
        setup
    };
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&setup, mock.endpoints()).unwrap();
    adapter
        .connect(Arc::new(Recorder::default()) as _)
        .await
        .unwrap();
    adapter
        .send(&Outgoing::text("100", None, "hello"))
        .await
        .unwrap();
    adapter.disconnect().await;
}

fn profile_reopen(home: &std::path::Path) -> AdapterSetup {
    let layout = Home::open(home).unwrap();
    let dir = layout
        .profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    let config = scope.config().gateway.platform("discord");
    AdapterSetup {
        scope,
        platform: "discord".into(),
        config,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_proxy_the_adapter_connects_directly() {
    // `gateway.trust_env` is unset, so ambient proxy variables (which a
    // test cannot set safely) are never consulted: the adapter is direct.
    let home = tempfile::tempdir().unwrap();
    let setup = profile(home.path(), true);
    assert_eq!(
        pmt_gateway::messaging::proxy::Proxy::of(&setup).unwrap(),
        pmt_gateway::messaging::proxy::Proxy::Direct
    );
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&setup, mock.endpoints()).unwrap();
    adapter
        .connect(Arc::new(Recorder::default()) as _)
        .await
        .unwrap();
    assert_eq!(mock.handshakes().0, 1);
    adapter.disconnect().await;
}

fn many(n: usize) -> Vec<pmt_gateway::messaging::MenuCommand> {
    (0..n)
        .map(|i| pmt_gateway::messaging::MenuCommand {
            name: format!("cmd{i}"),
            description: format!("command {i}"),
            takes_args: true,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn application_commands_are_registered_up_to_the_limit() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    adapter
        .connect(Arc::new(Recorder::default()) as _)
        .await
        .unwrap();
    adapter.register_commands(&many(104)).await.unwrap();
    let puts = mock.command_puts();
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].as_array().unwrap().len(), 100);
    assert_eq!(puts[0][1]["options"][0]["name"], "args");
    adapter.disconnect().await;
}

/// Spec: messaging-gateway, Platform start pings: Discord has no app-home
/// event, so its start ping is the `/start` command registered first.
#[tokio::test(flavor = "multi_thread")]
async fn the_start_command_is_registered_first_and_arrives_as_a_start_ping() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    adapter.register_commands(&many(3)).await.unwrap();
    assert_eq!(mock.command_puts()[0][0]["name"], "start");
    mock.dispatch(
        "INTERACTION_CREATE",
        &serde_json::json!({"id": "i2", "token": "tok", "type": 2, "channel_id": mock::DM,
            "user": {"id": mock::USER}, "data": {"name": "start"}}),
    );
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    assert_eq!(link.events.lock().unwrap()[0].text.trim(), "/start");
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_does_not_overwrite_application_commands() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let setup = profile_with(home.path(), true, "options = { slash_commands = false }\n");
    let adapter = Discord::new(&setup, mock.endpoints()).unwrap();
    adapter
        .connect(Arc::new(Recorder::default()) as _)
        .await
        .unwrap();
    adapter.register_commands(&many(5)).await.unwrap();
    assert!(mock.command_puts().is_empty());
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slash_command_runs_as_its_command_text() {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile(home.path(), true), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    mock.dispatch(
        "INTERACTION_CREATE",
        &serde_json::json!({"id": "i1", "token": "tok", "type": 2, "channel_id": mock::DM,
            "user": {"id": mock::USER}, "data": {"name": "model",
            "options": [{"name": "args", "type": 3, "value": "fast"}]}}),
    );
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    assert_eq!(link.events.lock().unwrap()[0].text, "/model fast");
    eventually(|| !mock.callbacks().is_empty()).await;
    let callback = &mock.callbacks()[0];
    assert_eq!(callback["type"], 4);
    assert_eq!(callback["data"]["flags"], 64);
    adapter.disconnect().await;
}

fn server_message(id: &str, channel: &str, text: &str, mentions_bot: bool) -> serde_json::Value {
    let mentions = if mentions_bot {
        serde_json::json!([{"id": mock::BOT}])
    } else {
        serde_json::json!([])
    };
    serde_json::json!({"id": id, "channel_id": channel, "guild_id": mock::GUILD, "content": text,
        "author": {"id": mock::USER, "username": "kit-user"}, "mentions": mentions,
        "timestamp": "2026-11-02T10:00:00+00:00", "attachments": []})
}

async fn connected(extra: &str) -> (tempfile::TempDir, Arc<DiscordMock>, Discord, Arc<Recorder>) {
    let home = tempfile::tempdir().unwrap();
    let mock = DiscordMock::start().await;
    let adapter = Discord::new(&profile_with(home.path(), true, extra), mock.endpoints()).unwrap();
    let link = Arc::new(Recorder::default());
    adapter.connect(Arc::clone(&link) as _).await.unwrap();
    (home, mock, adapter, link)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mention_creates_a_thread_named_after_the_message() {
    let (_home, mock, adapter, link) = connected("options = { require_mention = true }\n").await;
    let text = format!("<@{}> summarize the incident", mock::BOT);
    mock.dispatch(
        "MESSAGE_CREATE",
        &server_message("5001", mock::CHANNEL, &text, true),
    );
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let threads = mock.threads_of(mock::CHANNEL);
    assert_eq!(threads.len(), 1);
    let (thread, name) = &threads[0];
    assert_eq!(name, "summarize the incident");
    let event = link.events.lock().unwrap()[0].clone();
    assert_eq!(event.chat_id, mock::CHANNEL);
    assert_eq!(event.thread_id.as_deref(), Some(thread.as_str()));
    // The answer goes into the thread.
    adapter
        .send(&Outgoing::text(mock::CHANNEL, Some(thread), "on it"))
        .await
        .unwrap();
    let sent = mock.sent().pop().unwrap();
    assert_eq!(sent.thread_id.as_deref(), Some(thread.as_str()));
    // The conversation goes on in the thread without a mention.
    mock.dispatch(
        "MESSAGE_CREATE",
        &server_message("5002", thread, "and the root cause?", false),
    );
    eventually(|| link.events.lock().unwrap().len() == 2).await;
    let follow = link.events.lock().unwrap()[1].clone();
    assert_eq!(follow.thread_id.as_deref(), Some(thread.as_str()));
    // After the first turn the thread takes the session's title.
    adapter
        .thread_titled(mock::CHANNEL, thread, "Incident summary")
        .await;
    assert_eq!(
        mock.channel_name(thread).as_deref(),
        Some("Incident summary")
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_excluded_channel_stays_inline() {
    let extra = format!(
        "options = {{ no_thread_channels = [\"{}\"] }}\n",
        mock::CHANNEL
    );
    let (_home, mock, adapter, link) = connected(&extra).await;
    let text = format!("<@{}> summarize the incident", mock::BOT);
    mock.dispatch(
        "MESSAGE_CREATE",
        &server_message("5003", mock::CHANNEL, &text, true),
    );
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    assert!(mock.threads_of(mock::CHANNEL).is_empty());
    assert!(link.events.lock().unwrap()[0].thread_id.is_none());
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_thread_keeps_its_name_and_replies_stay_inline() {
    let (_home, mock, adapter, link) = connected("").await;
    let mut reply = server_message(
        "5004",
        mock::CHANNEL,
        &format!("<@{}> again", mock::BOT),
        true,
    );
    reply["message_reference"] = serde_json::json!({"message_id": "4999"});
    mock.dispatch("MESSAGE_CREATE", &reply);
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    assert!(
        mock.threads_of(mock::CHANNEL).is_empty(),
        "replies never open threads"
    );
    mock.dispatch(
        "MESSAGE_CREATE",
        &server_message(
            "5005",
            mock::CHANNEL,
            &format!("<@{}> deploy status", mock::BOT),
            true,
        ),
    );
    eventually(|| link.events.lock().unwrap().len() == 2).await;
    let (thread, _) = mock.threads_of(mock::CHANNEL)[0].clone();
    // Someone renames it before the first turn ends: the title is not applied.
    mock.rename_channel(&thread, "renamed by hand");
    adapter
        .thread_titled(mock::CHANNEL, &thread, "Session title")
        .await;
    assert_eq!(
        mock.channel_name(&thread).as_deref(),
        Some("renamed by hand")
    );
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_initiated_sends_to_a_forum_open_posts_titled_by_their_first_line() {
    let (_home, mock, adapter, _link) = connected("").await;
    let sent = adapter
        .send(&Outgoing::text(
            mock::FORUM,
            None,
            "Weekly report\nAll green.",
        ))
        .await
        .unwrap();
    let posts = mock.threads_of(mock::FORUM);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].1, "Weekly report");
    assert_eq!(sent.message_id.as_deref(), Some(posts[0].0.as_str()));
    // A reply to the post stays in it.
    adapter
        .send(&Outgoing::text(mock::FORUM, Some(&posts[0].0), "follow-up"))
        .await
        .unwrap();
    assert_eq!(mock.threads_of(mock::FORUM).len(), 1);
    let last = mock.sent().pop().unwrap();
    assert_eq!(last.thread_id.as_deref(), Some(posts[0].0.as_str()));
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn member_roles_and_channel_history_come_from_the_platform() {
    let (_home, mock, adapter, link) = connected("").await;
    let text = format!("<@{}> hi", mock::BOT);
    mock.dispatch(
        "MESSAGE_CREATE",
        &server_message("6001", mock::CHANNEL, &text, true),
    );
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let event = link.events.lock().unwrap()[0].clone();
    assert!(event.addressing.mentioned, "a literal mention");
    assert!(adapter.describe().exposes_roles && adapter.describe().can_fetch_history);
    assert!(
        adapter.member_roles(&event).await.is_err(),
        "unknown member: the lookup fails"
    );
    mock.set_roles(&event.sender_id, &["111222333"]);
    assert_eq!(adapter.member_roles(&event).await.unwrap(), ["111222333"]);
    assert_eq!(
        adapter.resolve_role("moderators").await.as_deref(),
        Some("111222333")
    );
    assert_eq!(adapter.resolve_role("nobody").await, None);
    mock.post_history(mock::CHANNEL, "1", mock::BOT, "earlier answer");
    mock.post_history(mock::CHANNEL, "2", mock::USER, "a new question");
    let history = adapter
        .fetch_history(mock::CHANNEL, None, 50)
        .await
        .unwrap();
    let seen: Vec<(String, bool)> = history
        .iter()
        .map(|h| (h.text.clone(), h.from_self))
        .collect();
    assert_eq!(
        seen,
        [
            ("earlier answer".to_owned(), true),
            ("a new question".to_owned(), false)
        ],
        "oldest first, the bot's own marked"
    );
}

/// Spec: messaging-gateway, Branching into a sibling thread: a named
/// thread opens in the channel and can be renamed.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_thread_opens_in_the_channel_and_can_be_renamed() {
    let (_home, mock, adapter, _link) = connected("").await;
    let thread = adapter
        .open_thread(mock::CHANNEL, "try-postgres")
        .await
        .unwrap();
    assert!(
        mock.threads_of(mock::CHANNEL)
            .contains(&(thread.clone(), "try-postgres".to_owned()))
    );
    adapter
        .rename_thread(mock::CHANNEL, &thread, "postgres plan")
        .await
        .unwrap();
    assert_eq!(mock.channel_name(&thread).as_deref(), Some("postgres plan"));
    adapter.disconnect().await;
}

/// A refusal to someone who pressed a button is an ephemeral follow-up of
/// their interaction; without one it is a direct message.
#[tokio::test(flavor = "multi_thread")]
async fn a_private_reply_is_an_ephemeral_follow_up_or_a_dm() {
    let (_home, mock, adapter, link) = connected("").await;
    mock.press("42", "appr:x:once:mac");
    eventually(|| !link.events.lock().unwrap().is_empty()).await;
    let pressed = link.events.lock().unwrap()[0].clone();
    assert_eq!(
        pressed.interaction.as_deref(),
        Some(format!("{}/itoken", mock::APPLICATION).as_str())
    );
    let mut refusal = pmt_gateway::messaging::Private {
        chat_id: mock::CHANNEL.into(),
        thread_id: None,
        sender_id: mock::USER.into(),
        interaction: pressed.interaction.clone(),
        text: "Only the person who asked or an admin may decide this.".into(),
    };
    adapter.reply_privately(&refusal).await.unwrap();
    assert_eq!(
        mock.private_replies(),
        [(mock::USER.to_owned(), refusal.text.clone())]
    );
    refusal.interaction = None;
    adapter.reply_privately(&refusal).await.unwrap();
    assert!(
        mock.sent()
            .iter()
            .any(|m| m.chat_id == mock::DM && m.text == refusal.text),
        "a direct message"
    );
}

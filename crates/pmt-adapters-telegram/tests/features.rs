//! Telegram features against the mock Bot API (spec: platform-adapters,
//! Native command menus and slash registration; Telegram webhook mode;
//! Telegram topics and per-topic skills).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pmt_adapters_telegram::mock::{BotApi, DM, TOKEN};
use pmt_adapters_telegram::{TOKEN_SECRET, Telegram};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, HttpResponse,
    Inbound, MenuCommand,
};
use serde_json::{Value, json};

#[derive(Default)]
struct Gateway {
    events: Mutex<Vec<Inbound>>,
    admitted: Vec<String>,
}

impl AdapterLink for Gateway {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.events.lock().unwrap().push(inbound);
        Box::pin(async { Admission::Accepted })
    }

    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}

    fn authorized<'a>(&'a self, sender_id: &'a str) -> AdapterFuture<'a, bool> {
        let admitted = self.admitted.iter().any(|a| a == sender_id);
        Box::pin(async move { admitted })
    }
}

impl Gateway {
    fn texts(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Fixture {
    home: tempfile::TempDir,
    api: Arc<BotApi>,
    bot: Arc<Telegram>,
    gateway: Arc<Gateway>,
}

async fn bot(options: &str, gateway: Gateway) -> Fixture {
    let api = BotApi::start().await;
    bot_with(options, gateway, api, &[]).await
}

async fn bot_with(
    options: &str,
    gateway: Gateway,
    api: Arc<BotApi>,
    extra: &[(&str, &str)],
) -> Fixture {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut secrets = vec![(TOKEN_SECRET, TOKEN)];
    secrets.extend_from_slice(extra);
    let table = format!("options = {{ poll_timeout_secs = 1{options} }}");
    let setup = kit_setup(home.path(), "telegram", &table, &secrets).unwrap();
    let bot = Arc::new(Telegram::new(&setup, api.endpoints()).unwrap());
    let gateway = Arc::new(gateway);
    bot.connect(Arc::clone(&gateway) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    Fixture {
        home,
        api,
        bot,
        gateway,
    }
}

fn catalog(n: usize) -> Vec<MenuCommand> {
    let mut all: Vec<MenuCommand> = (0..n)
        .map(|i| MenuCommand {
            name: format!("skill-{i:03}"),
            description: format!("skill number {i}"),
            takes_args: false,
        })
        .collect();
    for name in ["help", "model", "stop", "new"] {
        all.push(MenuCommand {
            name: name.into(),
            description: format!("the {name} command"),
            takes_args: false,
        });
    }
    all
}

#[tokio::test(flavor = "multi_thread")]
async fn priority_entries_survive_the_menu_cap() {
    let b = bot(
        ", command_menu = { priority = [\"new\", \"stop\", \"model\"] }",
        Gateway::default(),
    )
    .await;
    b.bot.register_commands(&catalog(136)).await.unwrap();
    let registered = b.api.calls("setMyCommands");
    let commands = registered[0]["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 60);
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    assert_eq!(names[..3], ["new", "stop", "model"]);
    assert_eq!(names[3], "skill_000", "Telegram names use underscores");
    b.bot.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follower_never_registers_the_menu() {
    let b = bot(", slash_commands = false", Gateway::default()).await;
    b.bot.register_commands(&catalog(10)).await.unwrap();
    assert!(b.api.calls("setMyCommands").is_empty());
    assert!(b.api.calls("deleteMyCommands").is_empty());
    b.bot.disconnect().await;
}

fn inline_query(from: i64, term: &str) -> Value {
    json!({"inline_query": {"id": format!("iq-{from}-{term}"), "from": {"id": from},
                            "query": term, "offset": ""}})
}

#[tokio::test(flavor = "multi_thread")]
async fn the_inline_picker_searches_everything_for_authorized_users_only() {
    let gateway = Gateway {
        admitted: vec![DM.to_string()],
        ..Gateway::default()
    };
    let b = bot("", gateway).await;
    b.bot.register_commands(&catalog(136)).await.unwrap();
    // skill_130 is beyond the 60-entry menu, yet the picker finds it.
    b.api.push(inline_query(DM, "skill_130"));
    b.api.push(inline_query(999, "skill"));
    eventually(|| b.api.calls("answerInlineQuery").len() == 2).await;
    let answers = b.api.calls("answerInlineQuery");
    let ours = answers
        .iter()
        .find(|a| a["inline_query_id"] == format!("iq-{DM}-skill_130"))
        .unwrap();
    assert_eq!(ours["results"][0]["title"], "/skill_130");
    let stranger = answers
        .iter()
        .find(|a| a["inline_query_id"] == "iq-999-skill")
        .unwrap();
    assert_eq!(stranger["results"], json!([]));
    assert!(
        b.gateway.texts().is_empty(),
        "inline queries are not messages"
    );
    b.bot.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn polling_deletes_any_webhook_first() {
    let b = bot("", Gateway::default()).await;
    eventually(|| b.api.methods().iter().any(|m| m == "getUpdates")).await;
    let methods = b.api.methods();
    let delete = methods.iter().position(|m| m == "deleteWebhook").unwrap();
    let poll = methods.iter().position(|m| m == "getUpdates").unwrap();
    assert!(delete < poll, "{methods:?}");
    b.bot.disconnect().await;
}

/// A local stand-in for the shared listener: unverified posts get 401.
async fn listener(slot: Arc<OnceLock<Arc<Telegram>>>) -> std::net::SocketAddr {
    pmt_gateway::messaging::mock_server::serve(move |request| {
        let slot = Arc::clone(&slot);
        async move {
            let Some(bot) = slot.get() else {
                return HttpResponse::status(503);
            };
            if !bot.verify_http(&request) {
                return HttpResponse::status(401);
            }
            bot.handle_http(&request).await
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_mode_admits_signed_updates_and_refuses_forged_ones() {
    let slot = Arc::new(OnceLock::new());
    let addr = listener(Arc::clone(&slot)).await;
    let url = format!("http://{addr}/p/default/telegram/");
    let options = format!(
        ", mode = \"webhook\", webhook_public_url = \"{url}\", webhook_secret_secret = \"TG_HOOK\""
    );
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let api = BotApi::start().await;
    let table = format!("options = {{ poll_timeout_secs = 1{options} }}");
    let setup = kit_setup(
        home.path(),
        "telegram",
        &table,
        &[(TOKEN_SECRET, TOKEN), ("TG_HOOK", "hook-secret-1")],
    )
    .unwrap();
    let bot = Arc::new(Telegram::new(&setup, api.endpoints()).unwrap());
    assert!(bot.describe().binds_http);
    let _ = slot.set(Arc::clone(&bot));
    let gateway = Arc::new(Gateway::default());
    bot.connect(Arc::clone(&gateway) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    assert_eq!(
        api.webhook(),
        Some((url.clone(), "hook-secret-1".to_owned()))
    );
    assert!(
        !api.methods().iter().any(|m| m == "getUpdates"),
        "no polling in webhook mode"
    );

    let forged = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
        .unwrap()
        .no_proxy()
        .build()
        .unwrap()
        .post(&url)
        .header("x-telegram-bot-api-secret-token", "wrong")
        .body(json!({"update_id": 1, "message": {"message_id": 1, "date": 1, "chat": {"id": DM, "type": "private"}, "from": {"id": DM}, "text": "forged"}}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 401);

    api.push(json!({"message": {"message_id": 7, "date": 1_700_000_000,
        "chat": {"id": DM, "type": "private"}, "from": {"id": DM, "first_name": "Kit"},
        "text": "hello by webhook"}}));
    eventually(|| !gateway.texts().is_empty()).await;
    assert_eq!(gateway.texts(), vec!["hello by webhook"]);
    bot.disconnect().await;
    drop(home);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dm_topic_is_created_once_and_preloads_its_skill() {
    let topics = format!(
        ", dm_topics = {{ \"{DM}\" = [{{ name = \"Research\", skill = \"arxiv\" }}, {{ name = \"Ops\", thread_id = 42 }}] }}"
    );
    let b = bot(&topics, Gateway::default()).await;
    let created = b.api.calls("createForumTopic");
    assert_eq!(
        created.len(),
        1,
        "only the topic without an id: {created:?}"
    );
    assert_eq!(created[0]["name"], "Research");
    let state = b
        .home
        .path()
        .join("profiles/default/platforms/telegram/topics.json");
    let recorded: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    let thread = recorded[DM.to_string()]["Research"].as_i64().unwrap();
    let config =
        std::fs::read_to_string(b.home.path().join("profiles/default/config.toml")).unwrap();
    assert!(
        !config.contains(&thread.to_string()),
        "config.toml is never rewritten"
    );

    let hints = b
        .bot
        .session_hints(&DM.to_string(), Some(&thread.to_string()));
    assert_eq!(hints.skills, vec!["arxiv"]);
    assert!(b.bot.session_hints(&DM.to_string(), None).is_empty());
    assert!(b.bot.session_hints(&DM.to_string(), Some("42")).is_empty());

    b.api
        .push(json!({"message": {"message_id": 9, "date": 1_700_000_000,
        "message_thread_id": thread, "is_topic_message": true,
        "chat": {"id": DM, "type": "private"}, "from": {"id": DM, "first_name": "Kit"},
        "text": "find papers"}}));
    eventually(|| !b.gateway.texts().is_empty()).await;
    let event = b.gateway.events.lock().unwrap()[0].clone();
    assert_eq!(event.thread_id, Some(thread.to_string()));
    // A reconnect with the id remembered creates nothing new.
    b.bot.disconnect().await;
    b.bot
        .connect(Arc::clone(&b.gateway) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    assert_eq!(b.api.calls("createForumTopic").len(), 1);
    b.bot.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_without_threads_skips_topic_creation_and_topics_made_elsewhere_are_found() {
    let api = BotApi::start().await;
    api.not_a_forum(&DM.to_string());
    let topics = format!(
        ", dm_topics = {{ \"{DM}\" = [{{ name = \"A\" }}, {{ name = \"B\" }}] }}, group_topics = {{ \"-100\" = [{{ name = \"Deploys\", skill = \"deploy\" }}] }}"
    );
    let b = bot_with(&topics, Gateway::default(), api, &[]).await;
    assert_eq!(
        b.api.calls("createForumTopic").len(),
        1,
        "the first refusal stops creation in that chat"
    );
    b.api
        .push(json!({"message": {"message_id": 3, "date": 1_700_000_000,
        "message_thread_id": 77, "is_topic_message": true,
        "chat": {"id": -100, "type": "supergroup", "is_forum": true},
        "from": {"id": 5, "first_name": "Ada"},
        "forum_topic_created": {"name": "Deploys", "icon_color": 7_322_096}}}));
    eventually(|| b.bot.session_hints("-100", Some("77")).skills == ["deploy"]).await;
    b.bot.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ignore_root_dm_still_accepts_slash_commands() {
    let b = bot(", ignore_root_dm = true", Gateway::default()).await;
    for (id, text) in [(1, "chatter"), (2, "/new")] {
        b.api
            .push(json!({"message": {"message_id": id, "date": 1_700_000_000,
            "chat": {"id": DM, "type": "private"}, "from": {"id": DM, "first_name": "Kit"},
            "text": text}}));
    }
    eventually(|| !b.gateway.texts().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(b.gateway.texts(), vec!["/new"]);
    b.bot.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_group_mention_of_the_bot_is_marked_addressed() {
    let b = bot("", Gateway::default()).await;
    for (id, text) in [(1, "lunch?"), (2, "@kit_bot when is lunch?")] {
        b.api.push(serde_json::json!({"message": {
            "message_id": id, "date": 1_700_000_000,
            "chat": {"id": -1001, "type": "supergroup"},
            "from": {"id": 7, "first_name": "Ann"},
            "text": text}}));
    }
    eventually(|| b.gateway.events.lock().unwrap().len() == 2).await;
    let events = b.gateway.events.lock().unwrap().clone();
    assert!(!events[0].addressing.mentioned);
    assert!(events[1].addressing.mentioned);
}

/// Spec: messaging-gateway, Branching into a sibling thread; Multi-session
/// DM topics: topics are opened and renamed on request.
#[tokio::test(flavor = "multi_thread")]
async fn topics_are_opened_and_renamed_on_request() {
    let f = bot("", Gateway::default()).await;
    let dm = DM.to_string();
    let thread = f.bot.open_thread(&dm, "Research").await.unwrap();
    f.bot
        .rename_thread(&dm, &thread, "Postgres plan")
        .await
        .unwrap();
    let topics = f.api.topics();
    let id: i64 = thread.parse().unwrap();
    assert_eq!(
        topics,
        [
            (dm.clone(), id, "Research".to_owned()),
            (dm.clone(), id, "Postgres plan".to_owned())
        ]
    );
    f.api.not_a_forum("-100777");
    assert!(f.bot.open_thread("-100777", "x").await.is_err());
    f.bot.disconnect().await;
}

/// Spec: messaging-gateway, Multi-session DM topics: declared only while
/// enabled and the bot has topic mode in private chats; otherwise DMs
/// fall back to one session each.
#[tokio::test(flavor = "multi_thread")]
async fn dm_topics_are_declared_only_where_the_bot_has_topic_mode() {
    let on = bot(", dm_topics = { enabled = true }", Gateway::default()).await;
    assert!(on.bot.describe().dm_topics);
    on.bot.disconnect().await;
    let off = bot("", Gateway::default()).await;
    assert!(!off.bot.describe().dm_topics, "not enabled");
    off.bot.disconnect().await;
    let api = BotApi::start().await;
    api.without_dm_topics();
    let none = bot_with(
        ", dm_topics = { enabled = true }",
        Gateway::default(),
        api,
        &[],
    )
    .await;
    assert!(!none.bot.describe().dm_topics, "the bot has no topic mode");
    none.bot.disconnect().await;
}

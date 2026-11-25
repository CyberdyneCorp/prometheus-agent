//! How the bot presents itself (spec: platform-adapters, Telegram
//! presentation options): the pin during a turn, the online/offline status
//! indicator, the backlog kept or dropped at a cold boot, and link
//! previews. Silent delivery is `notify.rs`'s.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_adapters_telegram::mock::{BotApi, DM, TOKEN};
use pmt_adapters_telegram::{TOKEN_SECRET, Telegram};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound, Outgoing,
    TurnState,
};
use serde_json::json;

#[derive(Default)]
struct Link {
    texts: Mutex<Vec<String>>,
}

impl AdapterLink for Link {
    fn submit(&self, inbound: Inbound) -> AdapterFuture<'_, Admission> {
        self.texts.lock().unwrap().push(inbound.text);
        Box::pin(async { Admission::Accepted })
    }
    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}
    fn authorized<'a>(&'a self, _: &'a str) -> AdapterFuture<'a, bool> {
        Box::pin(async { true })
    }
}

impl Link {
    fn texts(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }
}

struct Rig {
    home: tempfile::TempDir,
    api: Arc<BotApi>,
}

impl Rig {
    async fn new() -> Self {
        Self {
            home: tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap(),
            api: BotApi::start().await,
        }
    }

    /// The bot of this rig's profile with `options` (a fresh adapter
    /// each time, as a restart of the adapter makes).
    fn bot(&self, options: &str) -> Telegram {
        let table = format!("options = {{ poll_timeout_secs = 1{options} }}");
        let setup = kit_setup(
            self.home.path(),
            "telegram",
            &table,
            &[(TOKEN_SECRET, TOKEN)],
        )
        .unwrap();
        Telegram::new(&setup, self.api.endpoints()).unwrap()
    }

    fn message(&self, n: usize) {
        self.api.push(json!({"message": {
            "message_id": 900 + n, "date": 1_700_000_000,
            "chat": {"id": DM, "type": "private"},
            "from": {"id": DM, "first_name": "Kit", "is_bot": false},
            "text": format!("queued {n}")
        }}));
    }
}

async fn eventually(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn turn(running: bool) -> TurnState {
    TurnState {
        chat_id: DM.to_string(),
        thread_id: None,
        trigger: Some("55".to_owned()),
        running,
    }
}

#[tokio::test]
async fn the_triggering_message_is_pinned_silently_during_its_turn() {
    let rig = Rig::new().await;
    let off = rig.bot("");
    off.turn_state(&turn(true)).await;
    off.turn_state(&turn(false)).await;
    assert!(rig.api.calls("pinChatMessage").is_empty());
    let bot = rig.bot(", pin_during_turn = true");
    bot.turn_state(&turn(true)).await;
    let pins = rig.api.calls("pinChatMessage");
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0]["message_id"], 55);
    assert_eq!(pins[0]["disable_notification"], true);
    assert!(rig.api.calls("unpinChatMessage").is_empty());
    bot.turn_state(&turn(false)).await;
    let unpins = rig.api.calls("unpinChatMessage");
    assert_eq!(unpins.len(), 1);
    assert_eq!(unpins[0]["message_id"], 55);
}

#[tokio::test]
async fn the_status_indicator_shows_online_while_connected_and_offline_after_a_clean_stop() {
    let rig = Rig::new().await;
    let bot = rig.bot(", status_indicator = true, status_online = \"Here\"");
    bot.connect(Arc::new(Link::default())).await.unwrap();
    let set = rig.api.calls("setMyShortDescription");
    assert_eq!(set.len(), 1);
    assert_eq!(set[0]["short_description"], "Here");
    bot.disconnect().await;
    let set = rig.api.calls("setMyShortDescription");
    assert_eq!(set.len(), 2);
    assert_eq!(set[1]["short_description"], "Offline");
    // Without the option the description is never touched.
    let quiet = rig.bot("");
    quiet.connect(Arc::new(Link::default())).await.unwrap();
    quiet.disconnect().await;
    assert_eq!(rig.api.calls("setMyShortDescription").len(), 2);
}

/// Scenario: Backlog replay on request.
#[tokio::test]
async fn the_backlog_is_replayed_in_order_when_asked() {
    let rig = Rig::new().await;
    for n in 1..=5 {
        rig.message(n);
    }
    let bot = rig.bot(", drop_pending_on_cold_boot = false");
    let link = Arc::new(Link::default());
    bot.connect(Arc::clone(&link) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    eventually(|| link.texts().len() == 5).await;
    let expected: Vec<String> = (1..=5).map(|n| format!("queued {n}")).collect();
    assert_eq!(link.texts(), expected);
    bot.disconnect().await;
}

#[tokio::test]
async fn a_cold_boot_drops_the_backlog_and_a_later_start_keeps_it() {
    let rig = Rig::new().await;
    rig.message(1);
    rig.message(2);
    let bot = rig.bot("");
    let link = Arc::new(Link::default());
    bot.connect(Arc::clone(&link) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    assert_eq!(
        rig.api.calls("deleteWebhook")[0]["drop_pending_updates"],
        true
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(link.texts().is_empty(), "{:?}", link.texts());
    bot.disconnect().await;
    // Queued while the adapter was stopped; the daemon kept running.
    rig.message(3);
    let restarted = rig.bot("");
    let link = Arc::new(Link::default());
    restarted
        .connect(Arc::clone(&link) as Arc<dyn AdapterLink>)
        .await
        .unwrap();
    eventually(|| link.texts() == ["queued 3"]).await;
    assert_eq!(
        rig.api.calls("deleteWebhook")[1]["drop_pending_updates"],
        false
    );
    restarted.disconnect().await;
}

#[tokio::test]
async fn link_previews_can_be_disabled() {
    let rig = Rig::new().await;
    let message = Outgoing::text(&DM.to_string(), None, "see https://example.org");
    rig.bot("").send(&message).await.unwrap();
    rig.bot(", disable_link_previews = true")
        .send(&message)
        .await
        .unwrap();
    let sent = rig.api.calls("sendMessage");
    assert!(sent[0].get("link_preview_options").is_none(), "{}", sent[0]);
    assert_eq!(sent[1]["link_preview_options"]["is_disabled"], true);
}

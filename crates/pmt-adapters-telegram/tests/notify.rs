//! How a Telegram message notifies (spec: scheduled-jobs, Push
//! notification flag, cron threads and delivery confirmation): a message
//! the gateway marks silent goes out with `disable_notification`, text and
//! media alike; a final (notifying) message and one the gateway leaves to
//! the adapter notify.

use std::sync::Arc;

use pmt_adapters_telegram::mock::{BotApi, DM, TOKEN};
use pmt_adapters_telegram::{TOKEN_SECRET, Telegram};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{Adapter, Outgoing};
use pmt_store::gateway::outbox::OutboundAttachment;

struct Fixture {
    home: tempfile::TempDir,
    api: Arc<BotApi>,
    bot: Telegram,
}

async fn bot() -> Fixture {
    let api = BotApi::start().await;
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let setup = kit_setup(home.path(), "telegram", "", &[(TOKEN_SECRET, TOKEN)]).unwrap();
    let bot = Telegram::new(&setup, api.endpoints()).unwrap();
    Fixture { home, api, bot }
}

fn message(f: &Fixture, text: &str, notify: Option<bool>, with_photo: bool) -> Outgoing {
    let mut message = Outgoing::text(&DM.to_string(), None, text);
    message.notify = notify;
    if with_photo {
        let file = f.home.path().join("chart.png");
        std::fs::write(&file, b"\x89PNG\r\n\x1a\nfake").unwrap();
        message.attachments = vec![OutboundAttachment::new(&file.to_string_lossy())];
    }
    message
}

#[tokio::test]
async fn a_silent_message_disables_the_notification_for_text_and_media() {
    let f = bot().await;
    let silent = f
        .bot
        .send(&message(&f, "quiet digest", Some(false), true))
        .await
        .unwrap();
    let silent_id = silent.message_id.unwrap();
    assert_eq!(f.api.uploads(), vec!["sendPhoto".to_owned()]);
    let quiet = f.api.silent();
    assert_eq!(quiet.len(), 2, "the text and the photo: {quiet:?}");
    assert!(quiet.contains(&silent_id));
}

#[tokio::test]
async fn final_and_default_messages_notify() {
    let f = bot().await;
    f.bot
        .send(&message(&f, "scheduled digest", Some(true), true))
        .await
        .unwrap();
    f.bot
        .send(&message(&f, "a reply", None, false))
        .await
        .unwrap();
    assert_eq!(f.api.calls("sendMessage").len(), 2);
    assert!(f.api.silent().is_empty(), "{:?}", f.api.silent());
}

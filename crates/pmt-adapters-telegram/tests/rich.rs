//! Telegram rich messages against the mock Bot API (spec: platform-adapters,
//! Markup conversion per platform): with `options.rich_messages = true`
//! replies go out as raw Markdown through `sendRichMessage` up to 32768
//! characters, and fall back to `MarkdownV2` when Telegram refuses them.

use std::sync::Arc;

use pmt_adapters_telegram::mock::{BotApi, DM, TOKEN};
use pmt_adapters_telegram::{TOKEN_SECRET, Telegram};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{Adapter, Markup, Outgoing};
use serde_json::Value;

struct Fixture {
    _home: tempfile::TempDir,
    api: Arc<BotApi>,
    bot: Telegram,
}

async fn bot(rich: bool) -> Fixture {
    let api = BotApi::start().await;
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let table = format!("options = {{ rich_messages = {rich} }}");
    let setup = kit_setup(home.path(), "telegram", &table, &[(TOKEN_SECRET, TOKEN)]).unwrap();
    let bot = Telegram::new(&setup, api.endpoints()).unwrap();
    Fixture {
        _home: home,
        api,
        bot,
    }
}

fn reply(text: &str) -> Outgoing {
    Outgoing::text(&DM.to_string(), None, text)
}

const TABLE: &str = "| name | stars |\n|---|---|\n| prometheus | 5 |\n";

fn texts(calls: &[Value]) -> Vec<String> {
    calls
        .iter()
        .map(|c| c["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
async fn rich_replies_are_raw_markdown_through_send_rich_message_up_to_32768() {
    let f = bot(true).await;
    let descriptor = f.bot.describe();
    assert_eq!(
        descriptor.markup,
        Markup::Markdown,
        "the host sends Markdown"
    );
    assert_eq!(descriptor.max_message_length, 32_768);
    let long = format!("{TABLE}\n{}", "word ".repeat(2_000));
    f.bot.send(&reply(&long)).await.unwrap();
    let rich = f.api.calls("sendRichMessage");
    assert_eq!(rich.len(), 1, "one message for a 10k reply");
    let markdown = rich[0]["rich_message"]["markdown"].as_str().unwrap();
    assert!(markdown.starts_with(TABLE), "the table is sent as written");
    assert!(rich[0].get("parse_mode").is_none());
    assert!(f.api.calls("sendMessage").is_empty());
}

#[tokio::test]
async fn a_refused_rich_message_falls_back_to_markdown_v2_in_4096_parts() {
    let f = bot(true).await;
    f.api
        .refuse_rich(400, "Bad Request: can't parse the rich message");
    let long = format!("**Report**\n\n{}", "word ".repeat(2_000));
    f.bot.send(&reply(&long)).await.unwrap();
    assert_eq!(f.api.calls("sendRichMessage").len(), 1);
    let parts = f.api.calls("sendMessage");
    assert_eq!(parts.len(), 3, "10k characters in 4096-character parts");
    assert!(parts.iter().all(|p| p["parse_mode"] == "MarkdownV2"));
    assert!(texts(&parts)[0].starts_with("*Report*"));
    // Rich messages stay on: the refusal was this message's.
    assert_eq!(f.bot.describe().markup, Markup::Markdown);
}

#[tokio::test]
async fn a_reply_over_32768_skips_the_rich_call() {
    let f = bot(true).await;
    f.bot.send(&reply(&"word ".repeat(7_000))).await.unwrap();
    assert!(f.api.calls("sendRichMessage").is_empty());
    assert_eq!(f.api.calls("sendMessage").len(), 9);
}

#[tokio::test]
async fn a_bot_api_without_rich_messages_turns_them_off() {
    let f = bot(true).await;
    f.api.refuse_rich(404, "Not Found: method not found");
    f.bot.send(&reply("**one**")).await.unwrap();
    assert_eq!(texts(&f.api.calls("sendMessage")), vec!["*one*".to_owned()]);
    let descriptor = f.bot.describe();
    assert_eq!(descriptor.markup, Markup::Platform);
    assert_eq!(descriptor.max_message_length, 4096);
    f.bot.send(&reply("*two*")).await.unwrap();
    assert_eq!(f.api.calls("sendRichMessage").len(), 1, "not tried again");
}

#[tokio::test]
async fn rich_edits_carry_markdown_and_fall_back_to_markdown_v2() {
    let f = bot(true).await;
    let id = f
        .bot
        .send(&reply("draft"))
        .await
        .unwrap()
        .message_id
        .unwrap();
    f.bot.edit(&id, &reply(TABLE)).await.unwrap();
    let edits = f.api.calls("editMessageText");
    assert_eq!(edits[0]["rich_message"]["markdown"], TABLE);
    f.api
        .refuse_rich(400, "Bad Request: can't parse the rich message");
    f.bot.edit(&id, &reply("**final**")).await.unwrap();
    let edits = f.api.calls("editMessageText");
    let last = edits.last().unwrap();
    assert_eq!(last["text"], "*final*");
    assert_eq!(last["parse_mode"], "MarkdownV2");
}

#[tokio::test]
async fn without_rich_messages_replies_are_markdown_v2() {
    let f = bot(false).await;
    let descriptor = f.bot.describe();
    assert_eq!(descriptor.markup, Markup::Platform);
    assert_eq!(descriptor.max_message_length, 4096);
    f.bot.send(&reply("*rendered by the host*")).await.unwrap();
    assert!(f.api.calls("sendRichMessage").is_empty());
    assert_eq!(f.api.calls("sendMessage")[0]["parse_mode"], "MarkdownV2");
}

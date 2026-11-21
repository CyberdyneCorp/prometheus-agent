//! A self-hosted Bot API server (spec: platform-adapters, Telegram local
//! Bot API server): its endpoints, its 2048 MB file limit under the
//! gateway's own cap, and `--local` mode's file paths.

use pmt_adapters_telegram::mock::{BotApi, TOKEN};
use pmt_adapters_telegram::{Endpoints, TOKEN_SECRET, Telegram};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{Adapter, Attachment, MediaKind, SendError};

const MB: u64 = 1024 * 1024;

/// The bot with `options` (TOML members) and `extra` config tables, its
/// factory endpoints unreachable unless `options` name the mock.
fn bot(api: &BotApi, options: &str, extra: &str) -> (tempfile::TempDir, Telegram) {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let table = format!("options = {{ {options} }}\n{extra}");
    let setup = kit_setup(home.path(), "telegram", &table, &[(TOKEN_SECRET, TOKEN)]).unwrap();
    let public = if options.contains("api_base_url") {
        Endpoints {
            api: "http://127.0.0.1:9".into(),
            files: "http://127.0.0.1:9/file".into(),
        }
    } else {
        api.endpoints()
    };
    (home, Telegram::new(&setup, public).unwrap())
}

fn local(api: &BotApi) -> String {
    format!("api_base_url = \"{}\"", api.endpoints().api)
}

async fn open(bot: &Telegram, id: &str) -> Result<u64, SendError> {
    let attachment = Attachment::new(MediaKind::Video, id);
    bot.open_media(&attachment)
        .await
        .map(|stream| stream.declared_len.unwrap_or_default())
}

/// Scenario: Large file with local server.
#[tokio::test]
async fn a_self_hosted_server_accepts_a_300_mb_video_under_the_gateway_cap() {
    let api = BotApi::start().await;
    api.add_large_file("vid", vec![0; 16], 300 * MB);
    let (_home, public) = bot(&api, "", "[gateway.media]\nmax_inbound_bytes = 536870912\n");
    let refused = open(&public, "vid").await.unwrap_err();
    assert!(
        refused
            .describe()
            .contains("300 MB, over the 20 MB maximum")
            && refused.describe().contains("api_base_url"),
        "{refused:?}"
    );
    let (_home, hosted) = bot(
        &api,
        &local(&api),
        "[gateway.media]\nmax_inbound_bytes = 536870912\n",
    );
    // Accepted: the download starts (the mock serves a few bytes of it).
    assert!(open(&hosted, "vid").await.is_ok());
    // The gateway's own cap (128 MB by default) is the effective maximum.
    let (_home, capped) = bot(&api, &local(&api), "");
    let refused = open(&capped, "vid").await.unwrap_err();
    assert!(
        refused.describe().contains("over the 128 MB maximum"),
        "{refused:?}"
    );
}

#[tokio::test]
async fn local_mode_reads_the_servers_path_and_names_an_unreadable_one() {
    let api = BotApi::start().await;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let file = dir.path().join("video.mp4");
    std::fs::write(&file, b"moving pictures").unwrap();
    api.add_local_file("vid", &file.to_string_lossy(), 15);
    let missing = dir.path().join("gone.mp4");
    api.add_local_file("gone", &missing.to_string_lossy(), 15);
    let (_home, bot) = bot(&api, &format!("{}, local_mode = true", local(&api)), "");
    let attachment = Attachment::new(MediaKind::Video, "vid");
    let mut stream = bot.open_media(&attachment).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream.chunks).await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(bytes, b"moving pictures");
    let refused = open(&bot, "gone").await.unwrap_err();
    assert!(
        refused.describe().contains(&format!(
            "local Bot API file not readable: {}",
            missing.display()
        )),
        "{refused:?}"
    );
}

/// `gateway telegram logout` calls `logOut` on the public server.
#[tokio::test]
async fn log_out_calls_the_public_server() {
    let api = BotApi::start().await;
    let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())
        .unwrap()
        .build()
        .unwrap();
    pmt_adapters_telegram::log_out(&client, &api.endpoints(), TOKEN)
        .await
        .unwrap();
    assert_eq!(api.calls("logOut").len(), 1);
}

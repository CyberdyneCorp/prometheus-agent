//! Feishu documents (spec: platform-adapters, Feishu document comments and
//! meeting invitations; Feishu and Yuanbao tools; Platform-contributed
//! toolsets): comment turns and the rules that admit commenters (with the
//! `gateway feishu comment-rules` command), replies posted into the
//! thread in 4000-character pieces, meeting invitations, and the
//! `feishu_doc` / `feishu_drive` tools against the mock Open API.

#![cfg(feature = "gateway")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::catalog::{self, Bridge};
use common::catalog_feishu::{APP_ID, BOT, DM, Feishu, SECRET, USER, envelope};
use pmt_config::schema::{GatewayConfig, PlatformConfig};
use pmt_core::cancel::CancelToken;
use pmt_gateway::messaging::Outgoing;
use pmt_tools::descriptor::{CallContext, Executor};
use pmt_tools::platform_tools::{Context, Contributor, SecretReader};
use prometheus::platform_tools_host::feishu;
use serde_json::{Value, json};

fn reply(user: &str, text: &str) -> Value {
    json!({"reply_id": format!("r-{text}"), "user_id": user,
        "content": {"elements": [{"type": "text_run", "text_run": {"text": text}}]}})
}

/// Writes the profile's comment rules file.
fn rules(kit: &prometheus::plugins_cmd::adapter_plugin::PluginKit, rules: &Value) {
    let dir = kit.setup().scope.dir().join("plugin-data/feishu/files");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("comment_rules.json"), rules.to_string()).unwrap();
}

/// Scenario: Unauthorized commenter.
#[tokio::test(flavor = "multi_thread")]
async fn a_commenter_outside_the_document_allowlist_starts_no_turn() {
    let mock = Feishu::start().await;
    let bridge = Bridge::start(&[mock.server.host()]);
    let (kit, root) = mock.install(Some(&bridge), &json!({}));
    rules(
        &kit,
        &json!({"policy": "pairing", "documents": {"docx:doc1": {"policy": "allowlist", "allow_from": ["ou_alice"]}}}),
    );
    mock.state.lock().unwrap().thread = (0..15)
        .map(|i| {
            reply(
                if i % 2 == 0 { "ou_alice" } else { BOT },
                &format!("line {i}"),
            )
        })
        .collect();
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    mock.comment("ev-bob", "ou_bob");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        link.received().is_empty(),
        "bob is not on the document's allowlist"
    );
    // The command agrees, and alice is admitted.
    let home = root.path().join("home");
    let check = common::ok(&common::run(
        &home,
        &[
            "gateway",
            "feishu",
            "comment-rules",
            "check",
            "docx:doc1",
            "ou_bob",
        ],
    ));
    assert!(
        check.contains("exact:docx:doc1") && check.contains("DENIED"),
        "{check}"
    );
    mock.comment("ev-alice", "ou_alice");
    let turns = link.wait_for(1).await;
    assert_eq!(turns.len(), 1);
    let turn = &turns[0];
    assert_eq!(turn.chat_id, "comment:docx:doc1:c1:local");
    assert_eq!(turn.sender_id, "ou_alice");
    for part in [
        "Quarterly plan: ship the adapter.",
        "Quoted selection: \"ship the adapter\"",
        "line 14",
    ] {
        assert!(
            turn.text.contains(part),
            "missing {part:?} in {}",
            turn.text
        );
    }
    // A selection comment brings its last 12 messages, not more.
    assert!(!turn.text.contains("line 2\n"), "{}", turn.text);
    assert!(turn.text.contains("line 3"), "{}", turn.text);
    // The reply goes into the thread in pieces of 4000 characters.
    let long = "word ".repeat(1700);
    adapter
        .send(&Outgoing::text(&turn.chat_id, None, &long))
        .await
        .unwrap();
    let posted: Vec<(String, String, Value)> = mock
        .state
        .lock()
        .unwrap()
        .drive
        .iter()
        .filter(|(m, p, _)| {
            m == "POST" && p.starts_with("/open-apis/drive/v1/files/doc1/comments/c1/replies")
        })
        .cloned()
        .collect();
    assert_eq!(posted.len(), 3, "{posted:?}");
    let piece = posted[0].2["content"]["elements"][0]["text_run"]["text"]
        .as_str()
        .unwrap();
    assert!(piece.chars().count() <= 4000);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_commenter_on_a_whole_document_comment_gets_the_last_twenty_lines() {
    let mock = Feishu::start().await;
    let bridge = Bridge::start(&[mock.server.host()]);
    let (kit, root) = mock.install(Some(&bridge), &json!({}));
    {
        let mut state = mock.state.lock().unwrap();
        state.whole = true;
        state.thread = (0..25)
            .map(|i| reply("ou_carol", &format!("w{i}")))
            .collect();
    }
    let home = root.path().join("home");
    let args = [
        "gateway",
        "feishu",
        "comment-rules",
        "pairing",
        "add",
        "ou_carol",
    ];
    assert!(common::ok(&common::run(&home, &args)).contains("Approved ou_carol"));
    let status = common::ok(&common::run(
        &home,
        &["gateway", "feishu", "comment-rules", "status"],
    ));
    assert!(
        status.contains("absent: defaults apply") && status.contains("ou_carol"),
        "{status}"
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    mock.comment("ev-carol", "ou_carol");
    let turns = link.wait_for(1).await;
    let turn = &turns[0];
    assert_eq!(turn.chat_id, "comment:docx:doc1:c1:whole");
    assert!(
        turn.text.contains("w24") && turn.text.contains("w5\n"),
        "{}",
        turn.text
    );
    assert!(!turn.text.contains("w4\n"), "{}", turn.text);
    // The answer is a new whole-document comment; NO_REPLY posts nothing.
    adapter
        .send(&Outgoing::text(&turn.chat_id, None, "Done."))
        .await
        .unwrap();
    adapter
        .send(&Outgoing::text(&turn.chat_id, None, "NO_REPLY"))
        .await
        .unwrap();
    let added: Vec<Value> = mock
        .state
        .lock()
        .unwrap()
        .drive
        .iter()
        .filter(|(_, p, _)| p.ends_with("/new_comments"))
        .map(|(_, _, b)| b.clone())
        .collect();
    assert_eq!(
        added,
        [json!({"file_type": "docx", "reply_elements": [{"type": "text", "text": "Done."}]})]
    );
    // Withdrawn, carol is no longer answered.
    let args = [
        "gateway",
        "feishu",
        "comment-rules",
        "pairing",
        "remove",
        "ou_carol",
    ];
    assert!(common::ok(&common::run(&home, &args)).contains("Removed"));
    mock.comment("ev-carol-2", "ou_carol");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(link.received().len(), 1);
    adapter.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_meeting_invitation_needs_an_inviter_a_number_and_an_authorized_sender() {
    let mock = Feishu::start().await;
    let bridge = Bridge::start(&[mock.server.host()]);
    let (kit, _root) = mock.install(
        Some(&bridge),
        &json!({"dm_policy": "allowlist", "allow_from": [USER]}),
    );
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let invite = |id: &str, inviter: &str, number: &str| {
        envelope(
            id,
            "vc.bot.meeting_invited_v1",
            &json!({"meeting": {"id": "m1", "topic": "Weekly sync", "meeting_no": number},
                "inviter": {"id": {"open_id": inviter}, "user_name": "Kim"}}),
        )
    };
    mock.deliver(&invite("ev-no-number", USER, ""));
    mock.deliver(&invite("ev-stranger", "ou_stranger", "111222333"));
    mock.deliver(&invite("ev-ok", USER, "123456789"));
    let turns = link.wait_for(1).await;
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(
        (turns[0].chat_id.as_str(), turns[0].sender_id.as_str()),
        (USER, USER)
    );
    assert!(
        turns[0].text.contains("Meeting number: 123456789"),
        "{}",
        turns[0].text
    );
    // The answer reaches the inviter's direct chat by open id.
    adapter
        .send(&Outgoing::text(USER, None, "Joining."))
        .await
        .unwrap();
    assert_eq!(
        mock.state.lock().unwrap().sent.last().unwrap().chat_id,
        USER
    );
    let _ = DM;
    adapter.disconnect().await;
}

fn tools(base: &str, enabled: bool) -> Vec<pmt_tools::RegisteredTool> {
    let config: PlatformConfig = serde_json::from_value(
        json!({"enabled": enabled, "options": {"app_id": APP_ID, "api_base": base}}),
    )
    .unwrap();
    let dir = catalog::scratch();
    let secrets: SecretReader =
        Arc::new(|name| (name == "FEISHU_APP_SECRET").then(|| SECRET.to_owned()));
    let context = Context {
        profile_dir: dir.path(),
        config: &config,
        granted: None,
        secrets,
        network: &pmt_net::http::OutboundPolicy::default(),
    };
    feishu::Tools.tools(&context)
}

async fn call(
    tools: &[pmt_tools::RegisteredTool],
    name: &str,
    args: Value,
) -> Result<String, String> {
    let tool = tools.iter().find(|t| t.name() == name).unwrap();
    let Executor::Handler(handler) = &tool.executor else {
        panic!("not a handler")
    };
    let context = CallContext {
        call_id: "c".into(),
        session_id: "s".into(),
        cancel: CancelToken::new(),
    };
    handler
        .call(&context, args)
        .await
        .map(|r| serde_json::to_string(&r.content).unwrap())
        .map_err(|f| f.message)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_document_tools_read_list_and_comment_through_the_open_api() {
    let mock = Feishu::start().await;
    mock.state.lock().unwrap().thread = vec![reply("ou_a", "first")];
    let all = tools(&mock.server.base(), true);
    let names: Vec<&str> = all.iter().map(pmt_tools::RegisteredTool::name).collect();
    assert_eq!(
        names,
        [
            "feishu_doc_read",
            "feishu_drive_list_comments",
            "feishu_drive_list_comment_replies",
            "feishu_drive_reply_comment",
            "feishu_drive_add_comment"
        ]
    );
    let text = call(&all, "feishu_doc_read", json!({"doc_token": "doc1"}))
        .await
        .unwrap();
    assert!(text.contains("Quarterly plan"), "{text}");
    let listed = call(
        &all,
        "feishu_drive_list_comment_replies",
        json!({"file_token": "doc1", "comment_id": "c1", "page_size": 20}),
    )
    .await
    .unwrap();
    assert!(listed.contains("first"), "{listed}");
    let err = call(
        &all,
        "feishu_drive_list_comments",
        json!({"file_token": "doc1", "page_size": 500}),
    )
    .await
    .unwrap_err();
    assert!(err.contains("page_size must be 1 to 100"), "{err}");
    let err = call(&all, "feishu_doc_read", json!({"doc_token": "../../auth"}))
        .await
        .unwrap_err();
    assert!(err.contains("not a Feishu token"), "{err}");
    call(
        &all,
        "feishu_drive_reply_comment",
        json!({"file_token": "doc1", "comment_id": "c1", "content": "a<b"}),
    )
    .await
    .unwrap();
    let drive = mock.state.lock().unwrap().drive.clone();
    let (_, path, body) = drive.last().unwrap();
    assert!(path.contains("file_type=docx"), "{path}");
    assert_eq!(body["content"]["elements"][0]["text_run"]["text"], "a&lt;b");
    let listing = drive
        .iter()
        .find(|(_, p, _)| p.contains("/replies?"))
        .unwrap();
    assert!(
        listing.1.contains("page_size=20") && listing.1.contains("user_id_type=open_id"),
        "{}",
        listing.1
    );
}

/// Scenario: Toolset absent elsewhere (Feishu).
#[test]
fn a_profile_without_feishu_has_no_feishu_tool_and_feishu_brings_its_bundle() {
    prometheus::platform_tools_host::contribute_all();
    let dir = catalog::scratch();
    let secrets: SecretReader = Arc::new(|_| None);
    let network = pmt_net::http::OutboundPolicy::default();
    let build = |gateway: &GatewayConfig| {
        let builtins = pmt_tools::Builtins::standard(Vec::new()).unwrap();
        let mut registry =
            pmt_tools::ToolRegistry::new("default", Arc::new(builtins)).with_platforms(["feishu"]);
        pmt_tools::platform_tools::register(&mut registry, dir.path(), gateway, &secrets, &network);
        registry
    };
    let names = |r: &pmt_tools::ToolRegistry| -> Vec<String> {
        r.tools().map(|t| t.name().to_owned()).collect()
    };
    let cli = build(&GatewayConfig::default());
    assert!(
        names(&cli).iter().all(|n| !n.starts_with("feishu_")),
        "{:?}",
        names(&cli)
    );
    let mut gateway = GatewayConfig::default();
    gateway.platforms.insert(
        "feishu".into(),
        serde_json::from_value(json!({"enabled": true, "options": {"app_id": APP_ID}})).unwrap(),
    );
    let with = build(&gateway);
    for toolset in ["feishu_doc", "feishu_drive"] {
        assert!(with.toolset(toolset).is_some(), "{toolset} missing");
    }
    let bundle = with.members("platform-feishu").unwrap();
    for tool in ["feishu_doc_read", "feishu_drive_add_comment"] {
        assert!(bundle.contains(&tool.to_owned()), "{bundle:?}");
    }
}

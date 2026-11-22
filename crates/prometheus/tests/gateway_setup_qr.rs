//! `prometheus gateway setup feishu|weixin` (spec: platform-adapters,
//! Feishu and Lark adapter; Weixin personal WeChat adapter): the QR-scan
//! flows against mock registration and login services. Each stores its
//! credential in the secrets store only and writes the account's ids to
//! the platform options.

#![cfg(feature = "gateway")]

mod common;

use std::sync::{Arc, Mutex};

use common::catalog::{self, MockHttp, Reply, Request};
use serde_json::json;

fn config(kit: &prometheus::plugins_cmd::adapter_plugin::PluginKit) -> toml::Table {
    let text = std::fs::read_to_string(kit.setup().scope.dir().join("config.toml")).unwrap();
    text.parse().unwrap()
}

fn option(config: &toml::Table, platform: &str, key: &str) -> String {
    config["gateway"]["platforms"][platform]["options"][key]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn feishu_accounts(polls: Arc<Mutex<u32>>) -> MockHttp {
    MockHttp::start(Arc::new(move |request: &Request| {
        match (request.method.as_str(), request.route()) {
            ("POST", "/oauth/v1/app/registration") => match request.field("action").as_deref() {
                Some("init") => {
                    Reply::json(200, &json!({"supported_auth_methods": ["client_secret"]}))
                }
                Some("begin") => Reply::json(
                    200,
                    &json!({"device_code": "dc-1", "user_code": "ABCD",
                        "verification_uri_complete": "https://open.feishu.cn/page/launcher?user_code=ABCD",
                        "interval": 1, "expire_in": 60}),
                ),
                Some("poll") if request.field("device_code").as_deref() == Some("dc-1") => {
                    let mut n = polls.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        Reply::json(400, &json!({"error": "authorization_pending"}))
                    } else {
                        Reply::json(
                            200,
                            &json!({"client_id": "cli_new", "client_secret": "fresh-app-secret",
                                "user_info": {"open_id": "ou_me", "tenant_brand": "lark"}}),
                        )
                    }
                }
                _ => Reply::json(400, &json!({"error": "invalid_request"})),
            },
            ("POST", "/open-apis/auth/v3/tenant_access_token/internal") => Reply::json(
                200,
                &json!({"code": 0, "tenant_access_token": "t-new", "expire": 7200}),
            ),
            ("GET", "/open-apis/bot/v3/info")
                if request.header("authorization") == Some("Bearer t-new") =>
            {
                Reply::json(
                    200,
                    &json!({"code": 0, "bot": {"open_id": "ou_bot", "app_name": "Kit Bot"}}),
                )
            }
            _ => Reply::status(404),
        }
    }))
}

#[test]
fn feishu_setup_creates_the_app_by_qr_and_stores_its_secret() {
    let polls = Arc::new(Mutex::new(0));
    let mock = feishu_accounts(Arc::clone(&polls));
    let options =
        json!({"app_id": "cli_placeholder", "accounts_url": mock.base(), "api_base": mock.base()});
    let (kit, root) = catalog::install("feishu", &options, &[]);
    let home = root.path().join("home");
    let out = common::ok(&common::run(&home, &["gateway", "setup", "feishu"]));
    assert!(
        out.contains("user_code=ABCD"),
        "the QR's link is shown: {out}"
    );
    assert!(out.contains("Created the lark app cli_new"), "{out}");
    assert!(out.contains("Bot: Kit Bot"), "{out}");
    assert!(
        !out.contains("fresh-app-secret"),
        "a secret was printed: {out}"
    );
    assert_eq!(*polls.lock().unwrap(), 2, "polled until approved");
    let listed = common::ok(&common::run(&home, &["secrets", "list"]));
    assert!(listed.contains("FEISHU_APP_SECRET"), "{listed}");
    let config = config(&kit);
    assert_eq!(option(&config, "feishu", "app_id"), "cli_new");
    assert_eq!(
        option(&config, "feishu", "domain"),
        "lark",
        "a Lark tenant switches the domain"
    );
    let text = std::fs::read_to_string(kit.setup().scope.dir().join("config.toml")).unwrap();
    assert!(!text.contains("fresh-app-secret"));
}

#[test]
fn weixin_setup_logs_in_by_qr_refreshing_an_expired_code() {
    let qrs = Arc::new(Mutex::new(0u32));
    let base = Arc::new(Mutex::new(String::new()));
    let (counter, seen_base) = (Arc::clone(&qrs), Arc::clone(&base));
    let mock = MockHttp::start(Arc::new(move |request: &Request| match request.route() {
        "/ilink/bot/get_bot_qrcode" => {
            let mut n = counter.lock().unwrap();
            *n += 1;
            Reply::json(
                200,
                &json!({"qrcode": format!("qr{n}"), "qrcode_img_content": format!("https://liteapp.weixin.qq.com/q/qr{n}")}),
            )
        }
        "/ilink/bot/get_qrcode_status" if request.path.ends_with("qrcode=qr1") => {
            Reply::json(200, &json!({"status": "expired"}))
        }
        "/ilink/bot/get_qrcode_status" if request.path.ends_with("qrcode=qr2") => Reply::json(
            200,
            &json!({"status": "confirmed", "ilink_bot_id": "acc1@im.bot", "bot_token": "wx-secret-token",
                    "baseurl": *seen_base.lock().unwrap(), "ilink_user_id": "u1"}),
        ),
        _ => Reply::status(404),
    }));
    *base.lock().unwrap() = mock.base().to_owned();
    let options = json!({"account_id": "placeholder", "base_url": mock.base()});
    let (kit, root) = catalog::install("weixin", &options, &[]);
    let home = root.path().join("home");
    let out = common::ok(&common::run(&home, &["gateway", "setup", "weixin"]));
    assert!(
        out.contains("liteapp.weixin.qq.com/q/qr2"),
        "the refreshed QR is shown: {out}"
    );
    assert!(
        out.contains("Logged in WeChat account acc1@im.bot"),
        "{out}"
    );
    assert!(
        !out.contains("wx-secret-token"),
        "the token was printed: {out}"
    );
    assert_eq!(*qrs.lock().unwrap(), 2);
    let listed = common::ok(&common::run(&home, &["secrets", "list"]));
    assert!(listed.contains("WEIXIN_TOKEN"), "{listed}");
    assert_eq!(option(&config(&kit), "weixin", "account_id"), "acc1@im.bot");
}

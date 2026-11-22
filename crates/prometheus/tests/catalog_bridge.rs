//! The socket bridge sidecar of the catalog adapters (spec:
//! platform-adapters, Supervised sidecar adapters; Catalog adapter set):
//! it answers only its per-start token, connects only where the plugin's
//! network grant allows, and every adapter ships the same bridge.

#![cfg(feature = "gateway")]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::Path;

use common::catalog::{self, Bridge};
use serde_json::json;

#[test]
fn the_bridge_needs_its_token_and_connects_only_to_granted_hosts() {
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = echo.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in echo.incoming().flatten() {
            let mut writer = stream.try_clone().unwrap();
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let _ = writer.write_all(format!("echo {line}\r\n").as_bytes());
            }
        }
    });
    let bridge = Bridge::start(&[format!("127.0.0.1:{port}")]);
    let (status, _) = catalog::post(&format!("{}/open", bridge.base), &[], b"{}");
    assert_eq!(status, 401, "no token, no bridge");
    let (status, answer) = bridge.call("/open", &json!({"url": "tcp://127.0.0.2:6667"}));
    assert_eq!(status, 403, "{answer}");
    let (status, answer) = bridge.call("/open", &json!({"url": format!("tcp://127.0.0.1:{port}")}));
    assert_eq!(status, 200, "{answer}");
    let id = answer["id"].as_str().unwrap().to_owned();
    bridge.call("/send", &json!({"id": id, "data": "hello\r\n"}));
    let mut frames = Vec::new();
    for _ in 0..100 {
        let (_, received) = bridge.call("/recv", &json!({"id": id}));
        frames.extend(received["frames"].as_array().cloned().unwrap_or_default());
        if !frames.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(frames, [json!("echo hello")]);
}

/// A long poll through the bridge: one POST whose whole answer is one
/// frame, so the adapter never blocks on it.
#[test]
fn a_whole_request_answers_one_frame_with_status_and_body() {
    let api = catalog::MockHttp::start(std::sync::Arc::new(|request: &catalog::Request| {
        catalog::Reply::json(
            200,
            &json!({"method": request.method, "got": request.json()}),
        )
    }));
    let bridge = Bridge::start(&[api.host()]);
    let (status, answer) = bridge.call(
        "/open",
        &json!({"url": format!("{}/poll", api.base()), "method": "POST", "body": "{\"a\":1}",
            "headers": {"content-type": "application/json"}, "whole": true}),
    );
    assert_eq!(status, 200, "{answer}");
    let id = answer["id"].as_str().unwrap().to_owned();
    let mut frames = Vec::new();
    for _ in 0..100 {
        let (_, received) = bridge.call("/recv", &json!({"id": id}));
        frames.extend(received["frames"].as_array().cloned().unwrap_or_default());
        if !frames.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let frame: serde_json::Value = serde_json::from_str(frames[0].as_str().unwrap()).unwrap();
    assert_eq!(frame["status"], 200);
    let body: serde_json::Value = serde_json::from_str(frame["body"].as_str().unwrap()).unwrap();
    assert_eq!(body, json!({"method": "POST", "got": {"a": 1}}));
}

/// A redirect is followed only to a granted host: a granted host (or an
/// open redirect on it) cannot lead the bridge anywhere else.
#[test]
fn a_redirect_to_an_ungranted_host_is_not_followed() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let contacted = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&contacted);
    let elsewhere = catalog::MockHttp::start(std::sync::Arc::new(move |_: &catalog::Request| {
        seen.fetch_add(1, Ordering::SeqCst);
        catalog::Reply::json(200, &json!({"secret": "metadata"}))
    }));
    let target = format!("{}/latest/meta-data", elsewhere.base());
    let granted =
        catalog::MockHttp::start(std::sync::Arc::new(move |request: &catalog::Request| {
            let to = if request.route() == "/same" {
                "/final".to_owned()
            } else if request.route() == "/final" {
                return catalog::Reply::json(200, &json!({"followed": true}));
            } else {
                target.clone()
            };
            catalog::Reply {
                status: 302,
                headers: vec![("location".into(), to)],
                body: Vec::new(),
            }
        }));
    let bridge = Bridge::start(&[granted.host()]);
    let frames = |path: &str| {
        let (status, answer) = bridge.call(
            "/open",
            &json!({"url": format!("{}{path}", granted.base()), "whole": true}),
        );
        assert_eq!(status, 200, "{answer}");
        let id = answer["id"].as_str().unwrap().to_owned();
        for _ in 0..100 {
            let (_, received) = bridge.call("/recv", &json!({"id": id}));
            if !received["frames"].as_array().unwrap().is_empty() || received["open"] == false {
                return received;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("no answer");
    };
    let refused = frames("/away");
    assert_eq!(refused["frames"], json!([]), "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("may not connect"),
        "{refused}"
    );
    assert_eq!(
        contacted.load(Ordering::SeqCst),
        0,
        "the ungranted host was contacted"
    );
    // A redirect within the granted host is followed.
    let followed = frames("/same");
    let frame: serde_json::Value =
        serde_json::from_str(followed["frames"][0].as_str().unwrap()).unwrap();
    assert_eq!(frame["status"], 200, "{followed}");
}

#[test]
fn every_socket_adapter_ships_the_same_bridge() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/first-party");
    let canonical = root.join("adapter-kit/bridge");
    for plugin in [
        "irc",
        "homeassistant",
        "ntfy",
        "mattermost",
        "simplex",
        "dingtalk",
        "feishu",
        "wecom",
        "weixin",
        "qqbot",
        "yuanbao",
        "buzz",
        "photon",
    ] {
        // Photon's sidecar builds on the bridge but pins its own packages.
        let files: &[&str] = if plugin == "photon" {
            &["bridge.mjs"]
        } else {
            &["bridge.mjs", "package.json", "package-lock.json"]
        };
        for file in files {
            let shipped = std::fs::read(root.join(plugin).join("sidecar").join(file)).unwrap();
            let source = std::fs::read(canonical.join(file)).unwrap();
            assert!(
                shipped == source,
                "plugins/first-party/{plugin}/sidecar/{file} differs from adapter-kit/bridge/{file}"
            );
        }
    }
}

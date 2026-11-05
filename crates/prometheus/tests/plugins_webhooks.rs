//! Outbound webhooks (spec: plugin-system, Outbound webhooks): signed
//! deliveries a receiver can verify, and endpoints that never answer do
//! not slow the turn down.

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use self::common::{append_config, command, mock_home, ok, run, run_with_stdin, stderr, stdout};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// HMAC-SHA256 as a receiver computes it (RFC 2104, written out here).
fn hmac_hex(secret: &str, body: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut key = [0u8; 64];
    key[..secret.len()].copy_from_slice(secret.as_bytes());
    let inner = Sha256::new()
        .chain_update(key.map(|b| b ^ 0x36))
        .chain_update(body)
        .finalize();
    let mac = Sha256::new()
        .chain_update(key.map(|b| b ^ 0x5c))
        .chain_update(inner)
        .finalize();
    mac.iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_delivery_is_verifiable_by_the_receiver() {
    // Spec scenario "Signed delivery verifiable by receiver".
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let home = mock_home();
    append_config(
        home.path(),
        &format!(
            "\n[[hooks.outbound]]\nname = \"ci\"\nurl = \"{}/hook\"\nevents = [\"on_session_end\"]\nsecret_name = \"CI_HOOK_SECRET\"\n",
            server.uri()
        ),
    );
    run_with_stdin(
        home.path(),
        &["secrets", "set", "CI_HOOK_SECRET"],
        "s3cret\n",
    );
    let listed = ok(&run(home.path(), &["plugins", "hooks", "list"]));
    assert!(listed.contains("ci  "), "{listed}");
    assert!(listed.contains("on_session_end  signed"), "{listed}");
    let path = home.path().to_path_buf();
    let out = tokio::task::spawn_blocking(move || run(&path, &["-z", "hi"]))
        .await
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "{received:?}");
    let request = &received[0];
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(
        header("x-prometheus-signature-256"),
        format!("sha256={}", hmac_hex("s3cret", &request.body))
    );
    assert_eq!(header("x-prometheus-event"), "on_session_end");
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["delivery_id"], header("x-prometheus-delivery"));
    assert_eq!(body["hook_event_name"], "on_session_end");
    assert_eq!(body["profile"], "default");
}

#[test]
fn a_dead_endpoint_never_stalls_a_turn() {
    // Spec scenario "Dead endpoint never stalls a turn": the endpoint
    // accepts connections and never answers; 50 tool calls each queue a
    // delivery, and the turn does not wait for any of them.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let home = mock_home();
    append_config(
        home.path(),
        &format!(
            "\n[[hooks.outbound]]\nname = \"dead\"\nurl = \"http://{addr}/hook\"\nevents = [\"post_tool_call\"]\ntimeout_secs = 1\n"
        ),
    );
    let mut rules = String::from("[[rule]]\nuser = \"go\"\n");
    for n in 0..50 {
        use std::fmt::Write as _;
        let _ = write!(
            rules,
            "[[rule.tool_calls]]\nname = \"todo\"\narguments = {{ todos = [{{ id = \"t{n}\", content = \"step\", status = \"pending\" }}] }}\n"
        );
    }
    rules.push_str("\n[[rule]]\nrepeat = true\ntext = \"all done\"\n");
    let script = home.path().join("rules.toml");
    std::fs::write(&script, rules).unwrap();
    let started = Instant::now();
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", script.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let took = started.elapsed();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "all done\n");
    // Delivering them in line would take 50 × (1 s + 1 s pause + 1 s); the
    // run takes the turn plus at most the 5 s exit flush.
    assert!(took < Duration::from_secs(30), "took {took:?}");
    let log = std::fs::read_to_string(home.path().join("profiles/default/logs/prometheus.log"))
        .unwrap_or_default();
    assert!(log.contains("webhook delivery failed"), "{log}");
    drop(listener);
}

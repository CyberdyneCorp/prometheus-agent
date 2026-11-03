//! The relay connector against a local connector fixture (spec:
//! messaging-gateway, Relay connector; agent-interop, `send_message` tool).

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_core::secret::SecretString;
use pmt_gateway::messaging::{AdapterState, Admission, RelayOptions};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use crate::support::{Harness, Logs, Setup, eventually};

/// How the fixture answers `egress` requests.
#[derive(Debug, Clone, Copy)]
enum Egress {
    Allow,
    Deny,
    Fault,
}

/// A connector that fronts `slack`, records every frame and the bearer
/// token, and lets the test push inbound frames.
struct Connector {
    url: String,
    frames: Arc<Mutex<Vec<Value>>>,
    authorization: Arc<Mutex<Option<String>>>,
    inbound: mpsc::UnboundedSender<Value>,
}

impl Connector {
    async fn start(egress: Egress) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/gateway", listener.local_addr().unwrap());
        let frames = Arc::new(Mutex::new(Vec::new()));
        let authorization = Arc::new(Mutex::new(None));
        let (inbound, mut pushed) = mpsc::unbounded_channel::<Value>();
        let (seen, auth) = (Arc::clone(&frames), Arc::clone(&authorization));
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            #[allow(clippy::result_large_err)] // the handshake callback's fixed signature
            let header = |req: &Request, resp: Response| {
                *auth.lock().unwrap() = req
                    .headers()
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_owned());
                Ok(resp)
            };
            let socket = tokio_tungstenite::accept_hdr_async(tcp, header)
                .await
                .unwrap();
            let (mut sink, mut stream) = socket.split();
            let descriptor = json!({
                "type": "descriptor", "contract_version": 1, "platforms": ["slack"],
                "max_message_length": 39000, "supports_edit": false, "extra": "ignored"
            });
            sink.send(Message::text(descriptor.to_string()))
                .await
                .unwrap();
            loop {
                tokio::select! {
                    Some(frame) = pushed.recv() => {
                        sink.send(Message::text(frame.to_string())).await.unwrap();
                    }
                    Some(Ok(Message::Text(text))) = stream.next() => {
                        let frame: Value = serde_json::from_str(&text).unwrap();
                        seen.lock().unwrap().push(frame.clone());
                        if let Some(answer) = answer(&frame, egress) {
                            sink.send(Message::text(answer.to_string())).await.unwrap();
                        }
                    }
                    else => return,
                }
            }
        });
        Self {
            url,
            frames,
            authorization,
            inbound,
        }
    }

    fn options(&self) -> RelayOptions {
        RelayOptions {
            url: self.url.clone(),
            token: SecretString::new("relay-credential".to_owned()),
            gateway_id: Some("gw-1".into()),
            allow_direct_platforms: Vec::new(),
            default_profile: "default".into(),
            extra_ca_file: None,
        }
    }

    fn frames_of(&self, kind: &str) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f["type"] == kind)
            .cloned()
            .collect()
    }
}

fn answer(frame: &Value, egress: Egress) -> Option<Value> {
    let id = frame["id"].clone();
    match (frame["type"].as_str()?, egress) {
        ("send", _) => Some(json!({"type": "result", "id": id, "ok": true, "message_id": "s1"})),
        ("egress", Egress::Allow) => {
            Some(json!({"type": "result", "id": id, "ok": true, "allow": true}))
        }
        ("egress", Egress::Deny) => {
            Some(json!({"type": "result", "id": id, "ok": true, "allow": false}))
        }
        ("egress", Egress::Fault) => Some(json!({
            "type": "result", "id": id, "ok": false,
            "error": {"kind": "internal", "message": "policy engine crashed"}
        })),
        _ => None,
    }
}

/// A profile with a direct Slack adapter enabled and Slack's home channel.
fn slack_profile() -> String {
    "[gateway.busy]\ndebounce_ms = 0\n\n[gateway.platforms.slack]\nenabled = true\n\
     [gateway.platforms.slack.home_channel]\nchat_id = \"C-home\"\n"
        .to_owned()
}

async fn relayed(connector: &Connector) -> (Harness, Logs, tracing::subscriber::DefaultGuard) {
    let (logs, guard) = Logs::capture();
    let mut setup = Setup::new(&slack_profile());
    setup.relay = Some(connector.options());
    setup.unconnected = vec!["slack"];
    let h = Harness::start(setup).await;
    eventually("the relayed platform connects", || {
        h.gateway
            .statuses()
            .iter()
            .any(|s| s.platform == "slack" && s.state == AdapterState::Connected)
    })
    .await;
    (h, logs, guard)
}

#[tokio::test]
async fn the_relay_fronts_slack_instead_of_the_direct_adapter() {
    let connector = Connector::start(Egress::Allow).await;
    let (h, logs, _guard) = relayed(&connector).await;
    assert!(
        h.adapter("slack").link.lock().unwrap().is_none(),
        "the direct Slack adapter never connects"
    );
    assert!(
        logs.text().contains("the relay fronts slack"),
        "{}",
        logs.text()
    );
    assert_eq!(
        connector.authorization.lock().unwrap().as_deref(),
        Some("Bearer relay-credential")
    );
    assert!(
        !logs.text().contains("relay-credential"),
        "the credential is never logged"
    );
}

#[tokio::test]
async fn a_relayed_sender_is_trusted_upstream_and_answered_through_the_relay() {
    let connector = Connector::start(Egress::Allow).await;
    let (h, _logs, _guard) = relayed(&connector).await;
    connector
        .inbound
        .send(json!({
            "type": "inbound", "platform": "slack", "message_id": "m1",
            "sender_id": "U-stranger", "chat_id": "D-1", "chat_type": "dm", "text": "hello"
        }))
        .unwrap();
    eventually("the admission is reported", || {
        connector
            .frames_of("admission")
            .iter()
            .any(|f| f["admission"] == json!(Admission::Accepted))
    })
    .await;
    eventually("the answer goes back through the relay", || {
        connector.frames_of("send").iter().any(|f| {
            f["text"] == "echo: hello" && f["platform"] == "slack" && f["chat_id"] == "D-1"
        })
    })
    .await;
    assert_eq!(h.agents.texts(), vec!["hello"]);
}

#[tokio::test]
async fn the_send_tool_asks_the_relay_and_fails_closed() {
    for (egress, allowed) in [
        (Egress::Allow, true),
        (Egress::Deny, false),
        (Egress::Fault, false),
    ] {
        let connector = Connector::start(egress).await;
        let (h, _logs, _guard) = relayed(&connector).await;
        let to = h.gateway.resolve_target("default", "slack").await.unwrap();
        let verdict = h.gateway.authorize_destination("default", &to).await;
        assert_eq!(verdict.is_ok(), allowed, "{egress:?}: {verdict:?}");
        if let Err(err) = verdict {
            assert_eq!(
                err.message(),
                "target slack:C-home is not an authorized destination"
            );
        }
        assert_eq!(connector.frames_of("egress").len(), 1, "{egress:?}");
    }
}

#[tokio::test]
async fn an_allowed_direct_platform_keeps_its_own_adapter() {
    let connector = Connector::start(Egress::Allow).await;
    let mut setup = Setup::new(&slack_profile());
    let mut options = connector.options();
    options.allow_direct_platforms = vec!["slack".into()];
    setup.relay = Some(options);
    let h = Harness::start(setup).await;
    assert!(
        h.adapter("slack").link.lock().unwrap().is_some(),
        "the direct Slack adapter connects"
    );
}

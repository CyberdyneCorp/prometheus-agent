//! The A2A server of a test daemon: a home whose default profile serves
//! A2A on a loopback port, peer tokens kept in its secrets store, and
//! JSON-RPC calls over the minimal HTTP client of [`super::api`].

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};

use super::api::{Answer, Request};
use super::{append_config, ok, run_with_stdin};

/// The bearer token of peer `name`.
pub fn token_of(peer: &str) -> String {
    format!("a2a-token-{peer}-0123456789abcdef")
}

/// The secret holding peer `name`'s token.
pub fn secret_of(peer: &str) -> String {
    format!("A2A_PEER_{}", peer.to_ascii_uppercase())
}

/// Writes `daemon.toml` with the A2A listener on a loopback port 0.
pub fn listen(home: &Path) {
    std::fs::write(
        home.join("daemon.toml"),
        "[a2a]\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
}

/// Enables the A2A server in the default profile with a token for each
/// of `peers` (trusting `trusted`), and `extra` server keys.
pub fn enable(home: &Path, peers: &[&str], trusted: &[&str], extra: &str) {
    let trusted: Vec<String> = trusted.iter().map(|p| format!("{p:?}")).collect();
    let mut text = format!(
        "\n[a2a.server]\nenabled = true\ntrusted_peers = [{}]\n{extra}\n",
        trusted.join(", ")
    );
    for peer in peers {
        let _ = writeln!(
            text,
            "[[a2a.server.peers]]\nname = {peer:?}\ntoken_secret = {:?}",
            secret_of(peer)
        );
        ok(&run_with_stdin(
            home,
            &["secrets", "set", &secret_of(peer)],
            &format!("{}\n", token_of(peer)),
        ));
    }
    append_config(home, &text);
}

/// The running daemon's A2A port.
pub fn port(home: &Path) -> u16 {
    super::daemon::bound_port(home, "listener:a2a")
}

/// A JSON-RPC call as `peer` (`None`: no token).
pub fn rpc(port: u16, peer: Option<&str>, method: &str, params: &Value) -> Answer {
    let body = json!({"jsonrpc": "2.0", "id": "1", "method": method, "params": params});
    let mut request = Request::new("POST", "/").json(&body);
    if let Some(peer) = peer {
        request = request.header("Authorization", &format!("Bearer {}", token_of(peer)));
    }
    request.send(port)
}

/// `SendMessage` params with `text` in `context`.
pub fn message(text: &str, context: Option<&str>) -> Value {
    let mut message = json!({"messageId": ulid(), "role": "ROLE_USER", "parts": [{"text": text}]});
    if let Some(context) = context {
        message["contextId"] = context.into();
    }
    json!({"message": message})
}

fn ulid() -> String {
    format!(
        "m-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

//! An adapter host (spec: platform-adapters, Adapter host forwarding): its
//! adapters are only as good as its upstream, so until the upstream
//! connection is up they report `degraded` with `upstream_unreachable`,
//! also when they connected before forwarding started.

use pmt_core::secret::SecretString;
use pmt_gateway::messaging::{AdapterState, ForwardOptions};

use crate::support::{Harness, Setup, eventually, open};

/// An upstream that accepts the TCP connection and never answers the
/// WebSocket handshake keeps the host's adapters out of `connected`.
#[tokio::test]
async fn adapters_are_degraded_until_the_upstream_connection_is_up() {
    let stalled = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut setup = Setup::new(&open("telegram"));
    setup.forward = Some(ForwardOptions {
        url: format!("ws://{}/adapters", stalled.local_addr().unwrap()),
        token: SecretString::new("pmtd_host"),
        extra_ca_file: None,
    });
    let h = Harness::start(setup).await;
    eventually("the adapter reports the upstream down", || {
        h.gateway.statuses().iter().all(|s| {
            s.state == AdapterState::Degraded && s.code.as_deref() == Some("upstream_unreachable")
        })
    })
    .await;
    drop(stalled);
    h.stop().await;
}

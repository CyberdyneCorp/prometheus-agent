//! `network.proxy` and loopback (spec: local-models, Local inference stays
//! local): a model client reaches a server on this machine directly, while
//! every other client keeps sending loopback requests through the proxy.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_net::OutboundPolicy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A loopback HTTP server answering every request with `direct`.
async fn server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    pmt_core::spawn::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\ndirect",
                )
                .await;
        }
    });
    port
}

/// A proxy that counts connections and drops them (no network behind it).
async fn counting_proxy() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    pmt_core::spawn::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (url, seen)
}

#[tokio::test]
async fn a_loopback_direct_client_skips_the_proxy_for_loopback_only() {
    let port = server().await;
    let (proxy, seen) = counting_proxy().await;
    let policy = OutboundPolicy {
        proxy: Some(proxy),
        ..OutboundPolicy::default()
    };

    let direct = pmt_net::client_builder_loopback_direct(&policy)
        .unwrap()
        .build()
        .unwrap();
    for host in ["127.0.0.1", "localhost"] {
        let body = direct
            .get(format!("http://{host}:{port}/v1/models"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "direct", "{host}");
    }
    assert_eq!(seen.load(Ordering::SeqCst), 0, "loopback went to the proxy");

    // A remote host still goes through the proxy (which drops it).
    let _ = direct.get("http://provider.test/v1/models").send().await;
    let after_remote = seen.load(Ordering::SeqCst);
    assert!(after_remote > 0, "a remote host skipped the proxy");

    // Any other client proxies loopback as configured.
    let proxied = pmt_net::client(&policy).unwrap();
    let _ = proxied
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .send()
        .await;
    assert!(
        seen.load(Ordering::SeqCst) > after_remote,
        "the plain client skipped the proxy"
    );
}

//! The opt-in, content-free health export (spec: messaging-gateway,
//! Gateway health export is opt-in and content-free).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_gateway::messaging::health::HealthOptions;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::support::{Harness, Logs, Setup, eventually, open, said};

/// An OTLP/HTTP collector that keeps every request body.
async fn collector() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&bodies);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(serve(tcp, Arc::clone(&kept)));
        }
    });
    (url, bodies)
}

/// Answers every request of one connection with 200, keeping its body.
async fn serve(mut tcp: tokio::net::TcpStream, kept: Arc<Mutex<Vec<String>>>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(n) = tcp.read(&mut chunk).await {
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
        while let Some(body) = take_request(&mut buffer) {
            kept.lock().unwrap().push(body);
            let _ = tcp
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    }
}

/// Removes one complete request from `buffer` and returns its body.
fn take_request(buffer: &mut Vec<u8>) -> Option<String> {
    let text = String::from_utf8_lossy(buffer).into_owned();
    let end = text.find("\r\n\r\n")?;
    let length: usize = text[..end]
        .lines()
        .find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    if buffer.len() < end + 4 + length {
        return None;
    }
    let body = String::from_utf8_lossy(&buffer[end + 4..end + 4 + length]).into_owned();
    buffer.drain(..end + 4 + length);
    Some(body)
}

fn health(endpoint: &str, every: Duration) -> HealthOptions {
    HealthOptions {
        endpoint: endpoint.to_owned(),
        interval: every,
        extra_ca_file: None,
    }
}

#[tokio::test]
async fn the_export_carries_counts_but_no_content_or_identities() {
    let (url, bodies) = collector().await;
    let mut setup = Setup::new(&open("telegram"));
    setup.health = Some(health(&url, Duration::from_millis(100)));
    let h = Harness::start(setup).await;
    h.dm("telegram", "u-4242", "confidential plan").await;
    said(&h.adapter("telegram"), "echo: confidential plan").await;
    eventually("an export with the admission", || {
        bodies.lock().unwrap().iter().any(|b| {
            b.contains("prometheus.gateway.admissions") && b.contains("prometheus.outbox.sent")
        })
    })
    .await;
    let bodies = bodies.lock().unwrap().clone();
    let last = bodies.last().unwrap();
    for metric in [
        "prometheus.gateway.conversations",
        "prometheus.adapter.state",
        "prometheus.outbox.queued",
    ] {
        assert!(last.contains(metric), "{metric} in {last}");
    }
    for body in &bodies {
        assert!(!body.contains("confidential plan"), "message text exported");
        assert!(!body.contains("u-4242"), "user or chat id exported");
        let export: serde_json::Value = serde_json::from_str(body).unwrap();
        let metrics = &export["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        for metric in metrics.as_array().unwrap() {
            let name = metric["name"].as_str().unwrap();
            assert!(
                pmt_gateway::messaging::health::METRICS.contains(&name),
                "{name} is not allowlisted"
            );
        }
    }
}

#[tokio::test]
async fn an_unreachable_collector_changes_nothing_but_one_warning() {
    // A port nothing listens on.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/metrics", closed.local_addr().unwrap());
    drop(closed);
    let (logs, _guard) = Logs::capture();
    let mut setup = Setup::new(&open("telegram"));
    setup.health = Some(health(&url, Duration::from_millis(100)));
    let h = Harness::start(setup).await;
    for i in 0..3 {
        let started = tokio::time::Instant::now();
        h.dm("telegram", "7", &format!("ping {i}")).await;
        said(&h.adapter("telegram"), &format!("echo: ping {i}")).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a reply was delayed"
        );
    }
    eventually("an export was attempted", || {
        logs.text().contains("gateway health export failed")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let warnings = logs.text().matches("gateway health export failed").count();
    assert_eq!(
        warnings,
        1,
        "at most one warning a minute:\n{}",
        logs.text()
    );
}

#[tokio::test]
async fn nothing_is_exported_by_default() {
    let (_url, bodies) = collector().await;
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "7", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(bodies.lock().unwrap().is_empty());
}

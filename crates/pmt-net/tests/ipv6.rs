//! The two `IPv6 reachability check and IPv4-only outbound` scenarios
//! against local servers: a provider host whose AAAA record is blackholed
//! (the RFC 6666 discard prefix) or answered by a different listener.

use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use pmt_core::doctor::Status;
use pmt_net::OutboundPolicy;
use pmt_net::doctor::{BLACKHOLE_WARNING, Ipv6Check, Target};
use pmt_net::http::client_builder_with_lookup;
use pmt_net::resolve::{Lookup, LookupFuture};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const HOST: &str = "api.provider.test";
/// RFC 6666 discard-only prefix: packets to it are dropped (or, without an
/// IPv6 route, refused at once); either way IPv6 fails.
const BLACKHOLE: &str = "100::1";

/// A resolver that answers [`HOST`] with fixed AAAA and A records.
struct FixedLookup(Vec<IpAddr>);

impl Lookup for FixedLookup {
    fn lookup(&self, host: &str) -> LookupFuture {
        let answer = if host == HOST {
            Ok(self.0.clone())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, host.to_owned()))
        };
        Box::pin(async move { answer })
    }
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// Serves HTTP 200 with `body` on every connection.
fn serve(listener: TcpListener, body: &'static str) {
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
}

#[tokio::test]
async fn blackholed_ipv6_is_reported_naming_force_ipv4() {
    let v4 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = v4.local_addr().unwrap().port();
    serve(v4, "v4");
    let lookup = FixedLookup(vec![ip(BLACKHOLE), ip("127.0.0.1")]);
    let mut check = Ipv6Check {
        route: true,
        online: true,
        force_ipv4: false,
        targets: vec![Target {
            host: HOST.into(),
            port,
        }],
        timeout: Duration::from_millis(300),
    };

    let checks = check.run(&lookup).await;
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert_eq!(checks[1].status, Status::Warn);
    assert_eq!(checks[1].message, BLACKHOLE_WARNING);
    assert!(checks[1].message.contains("network.force_ipv4 = true"));

    check.force_ipv4 = true;
    let checks = check.run(&lookup).await;
    assert_eq!(checks[1].status, Status::Ok, "{checks:?}");
}

#[tokio::test]
async fn offline_check_only_reports_the_route() {
    let lookup = FixedLookup(vec![]);
    for route in [true, false] {
        let check = Ipv6Check {
            route,
            online: false,
            force_ipv4: false,
            targets: vec![Target {
                host: HOST.into(),
                port: 443,
            }],
            timeout: Duration::from_millis(300),
        };
        let checks = check.run(&lookup).await;
        assert_eq!(checks.len(), 1, "no probe without --online");
        assert_eq!(checks[0].name, "ipv6_route");
    }
}

#[tokio::test]
async fn force_ipv4_avoids_a_blackholed_aaaa_record() {
    let v4 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = v4.local_addr().unwrap().port();
    serve(v4, "v4");
    let lookup = Arc::new(FixedLookup(vec![ip(BLACKHOLE), ip("127.0.0.1")]));
    let client = client_builder_with_lookup(OutboundPolicy { force_ipv4: true }, lookup)
        // Far below the happy-eyeballs fallback a blackholed attempt would cost.
        .connect_timeout(Duration::from_millis(250))
        .build()
        .unwrap();

    let body = client
        .get(format!("http://{HOST}:{port}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "v4");
}

#[tokio::test]
async fn forced_ipv4_connects_only_to_a_records() {
    let v4 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = v4.local_addr().unwrap().port();
    serve(v4, "v4");
    // A listener on the AAAA address of the same port, when the host has ::1.
    let v6 = TcpListener::bind(("::1", port)).await.ok();
    let has_v6 = v6.is_some();
    if let Some(v6) = v6 {
        serve(v6, "v6");
    }
    let lookup: Arc<dyn Lookup> = Arc::new(FixedLookup(vec![ip("::1"), ip("127.0.0.1")]));
    let get = |force_ipv4: bool| {
        let lookup = Arc::clone(&lookup);
        async move {
            client_builder_with_lookup(OutboundPolicy { force_ipv4 }, lookup)
                .build()
                .unwrap()
                .get(format!("http://{HOST}:{port}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };

    assert_eq!(get(true).await, "v4");
    if has_v6 {
        // Control: without the setting the AAAA record is preferred, so
        // the assertion above really observed the filter.
        assert_eq!(get(false).await, "v6");
    }
}

#[tokio::test]
async fn ipv6_literal_is_refused_when_forced() {
    let Ok(v6) = TcpListener::bind("[::1]:0").await else {
        return; // no IPv6 loopback on this host
    };
    let port = v6.local_addr().unwrap().port();
    serve(v6, "v6");
    let client = client_builder_with_lookup(
        OutboundPolicy { force_ipv4: true },
        Arc::new(FixedLookup(vec![])),
    )
    .build()
    .unwrap();
    assert!(
        client
            .get(format!("http://[::1]:{port}/"))
            .send()
            .await
            .is_err()
    );
}

//! Reaching the Bot API when its host name does not resolve (spec:
//! platform-adapters, Telegram network resilience): fallback addresses,
//! DNS-over-HTTPS discovery bounded in time, TLS still validated for the
//! host name, and `fallback_ips = []` turning it all off.
//!
//! The mock Bot API sits behind a TLS front speaking as
//! `api.telegram.invalid` (a name that never resolves) with a certificate
//! of a private CA the profile trusts through `network.extra_ca_file`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_adapters_telegram::mock::{BotApi, TOKEN};
use pmt_adapters_telegram::{Endpoints, TOKEN_SECRET, Telegram};
use pmt_gateway::messaging::conformance::kit_setup;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, AdapterState, Admission, Inbound,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const HOST: &str = "api.telegram.invalid";

struct Link;

impl AdapterLink for Link {
    fn submit(&self, _: Inbound) -> AdapterFuture<'_, Admission> {
        Box::pin(async { Admission::Accepted })
    }
    fn report(&self, _: AdapterState, _: Option<AdapterError>) {}
    fn authorized<'a>(&'a self, _: &'a str) -> AdapterFuture<'a, bool> {
        Box::pin(async { true })
    }
}

/// A TLS front for `api` presenting a certificate for `name` from a
/// private CA: its loopback port and the CA's PEM.
async fn tls_front(api: &BotApi, name: &str) -> (u16, String) {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec![name.into()])
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::try_from(leaf_key.serialize_der()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![leaf.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let backend = api.endpoints().api.trim_start_matches("http://").to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    pmt_core::spawn::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, backend) = (acceptor.clone(), backend.clone());
            pmt_core::spawn::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut plain = tokio::net::TcpStream::connect(backend).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
            });
        }
    });
    (port, ca.pem())
}

/// A DNS-over-HTTPS resolver answering every question with `answer`
/// (after `delay`): its URL and the names asked.
async fn resolver(answer: &'static str, delay: Duration) -> (String, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/resolve", listener.local_addr().unwrap());
    let seen = Arc::clone(&asked);
    pmt_core::spawn::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            pmt_core::spawn::spawn(async move {
                let mut buf = vec![0; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                seen.lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or("").to_owned());
                tokio::time::sleep(delay).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/dns-json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    (url, asked)
}

struct Rig {
    _home: tempfile::TempDir,
    bot: Telegram,
}

/// The bot reaching `https://api.telegram.invalid:<port>` with `options`,
/// trusting `ca`, asking `doh` for more addresses.
fn bot(port: u16, ca: &str, options: &str, doh: Vec<String>) -> Rig {
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let ca_file = home.path().join("ca.pem");
    std::fs::write(&ca_file, ca).unwrap();
    let table = format!(
        "options = {{ {options} }}\n[network]\nextra_ca_file = \"{}\"\n",
        ca_file.display()
    );
    let setup = kit_setup(home.path(), "telegram", &table, &[(TOKEN_SECRET, TOKEN)]).unwrap();
    let endpoints = Endpoints {
        doh,
        ..Endpoints::at(&format!("https://{HOST}:{port}"))
    };
    Rig {
        bot: Telegram::new(&setup, endpoints).unwrap(),
        _home: home,
    }
}

/// Scenario: DNS blocked.
#[tokio::test]
async fn an_unresolvable_host_connects_through_a_fallback_address_checking_its_certificate() {
    let api = BotApi::start().await;
    let (port, ca) = tls_front(&api, HOST).await;
    let rig = bot(port, &ca, "fallback_ips = [\"127.0.0.1\"]", Vec::new());
    rig.bot.connect(Arc::new(Link)).await.unwrap();
    assert_eq!(api.calls("getMe").len(), 1);
    rig.bot.disconnect().await;
}

#[tokio::test]
async fn a_fallback_address_with_a_certificate_for_another_name_is_refused() {
    let api = BotApi::start().await;
    let (port, ca) = tls_front(&api, "elsewhere.invalid").await;
    let rig = bot(port, &ca, "fallback_ips = [\"127.0.0.1\"]", Vec::new());
    let refused = rig.bot.connect(Arc::new(Link)).await.unwrap_err();
    assert_eq!(refused.code, "telegram_unreachable", "{refused:?}");
    assert!(api.calls("getMe").is_empty());
}

#[tokio::test]
async fn an_empty_fallback_list_turns_the_fallback_off() {
    let api = BotApi::start().await;
    let (port, ca) = tls_front(&api, HOST).await;
    let (doh, asked) = resolver(
        r#"{"Answer":[{"type":1,"data":"127.0.0.1"}]}"#,
        Duration::ZERO,
    )
    .await;
    let rig = bot(port, &ca, "fallback_ips = []", vec![doh]);
    let refused = rig.bot.connect(Arc::new(Link)).await.unwrap_err();
    assert_eq!(refused.code, "telegram_unreachable", "{refused:?}");
    assert!(api.calls("getMe").is_empty());
    assert!(asked.lock().unwrap().is_empty(), "no discovery either");
}

#[tokio::test]
async fn discovery_over_https_extends_the_seed_addresses() {
    let api = BotApi::start().await;
    let (port, ca) = tls_front(&api, HOST).await;
    let (doh, asked) = resolver(
        r#"{"Answer":[{"type":1,"data":"127.0.0.1"}]}"#,
        Duration::ZERO,
    )
    .await;
    // The default seed addresses stay configured; discovery finds the
    // address that works.
    let rig = bot(port, &ca, "", vec![doh]);
    rig.bot.connect(Arc::new(Link)).await.unwrap();
    assert_eq!(api.calls("getMe").len(), 1);
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "asked once per connection: {asked:?}");
    assert!(
        asked[0].contains(&format!("name={HOST}")) && asked[0].contains("type=A"),
        "{asked:?}"
    );
    rig.bot.disconnect().await;
}

#[tokio::test]
async fn a_slow_resolver_is_given_up_on_after_the_discovery_timeout() {
    let api = BotApi::start().await;
    let (port, ca) = tls_front(&api, HOST).await;
    let (doh, _) = resolver(r#"{"Answer":[]}"#, Duration::from_mins(1)).await;
    let rig = bot(
        port,
        &ca,
        "fallback_ips = [\"127.0.0.1\"], fallback_discovery_timeout_secs = 1",
        vec![doh],
    );
    let started = Instant::now();
    rig.bot.connect(Arc::new(Link)).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(api.calls("getMe").len(), 1);
    rig.bot.disconnect().await;
}

//! The web dashboard of a test daemon: opening it through the CLI, signing
//! a client in with a login link, cookie-authenticated requests and the
//! dashboard's WebSocket endpoints.

use std::path::Path;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

use super::api::{Answer, Request};
use super::{ok, run};

/// What `prometheus dashboard --json` printed.
#[derive(Debug, Clone)]
pub struct Opened {
    pub url: String,
    pub login_url: String,
    pub port: u16,
    pub started: bool,
}

/// `prometheus [extra...] --json dashboard --no-open --port 0`.
pub fn open(home: &Path, extra: &[&str]) -> Opened {
    let mut args = extra.to_vec();
    args.extend(["--json", "dashboard", "--no-open", "--port", "0"]);
    parse(&ok(&run(home, &args)))
}

/// Parses `prometheus --json dashboard` output.
pub fn parse(text: &str) -> Opened {
    let doc: Value = serde_json::from_str(text).unwrap();
    let url = doc["url"].as_str().unwrap().to_owned();
    let port = url
        .rsplit(':')
        .next()
        .and_then(|rest| rest.trim_end_matches('/').split('/').next())
        .and_then(|p| p.parse().ok())
        .unwrap();
    Opened {
        url,
        login_url: doc["login_url"].as_str().unwrap_or_default().to_owned(),
        port,
        started: doc["started"].as_bool().unwrap_or_default(),
    }
}

/// The path and query of `url`.
pub fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.find('/')
        .map_or("/".to_owned(), |i| rest[i..].to_owned())
}

/// A signed-in client: the cookie header value and the CSRF token.
#[derive(Debug, Clone)]
pub struct Signed {
    pub cookie: String,
    pub csrf: String,
}

/// The `Set-Cookie` header of an answer.
pub fn set_cookie(answer: &Answer) -> Option<&str> {
    answer
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, v)| v.as_str())
}

/// Exchanges `opened`'s login link for a session.
pub fn sign_in(opened: &Opened) -> Signed {
    let answer = Request::new("GET", &path_of(&opened.login_url)).send(opened.port);
    assert_eq!(answer.status, 303, "{answer:?}");
    let cookie = set_cookie(&answer)
        .and_then(|c| c.split(';').next())
        .unwrap()
        .to_owned();
    let base = path_of(&opened.url);
    let session = get(
        opened.port,
        &format!("{}/api/session", base.trim_end_matches('/')),
        &cookie,
    );
    assert_eq!(session.status, 200, "{session:?}");
    Signed {
        csrf: session.json()["csrf"].as_str().unwrap().to_owned(),
        cookie,
    }
}

/// `GET path` with `cookie`.
pub fn get(port: u16, path: &str, cookie: &str) -> Answer {
    Request::new("GET", path)
        .header("Cookie", cookie)
        .send(port)
}

/// A state-changing call with the session's cookie and CSRF token.
pub fn call(port: u16, method: &str, path: &str, signed: &Signed, body: Option<&Value>) -> Answer {
    let request = Request::new(method, path)
        .header("Cookie", &signed.cookie)
        .header("X-Prometheus-CSRF", &signed.csrf);
    match body {
        Some(body) => request.json(body),
        None => request,
    }
    .send(port)
}

/// Opens a WebSocket of the dashboard at `path` with `headers` and the
/// `protocols` (a `Sec-WebSocket-Protocol` value).
pub async fn socket(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    protocols: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://127.0.0.1:{port}{path}")
        .into_client_request()
        .unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    if !protocols.is_empty() {
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(protocols).unwrap(),
        );
    }
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// Reads frames until the socket closes; its close code.
pub async fn close_code<S>(socket: &mut S) -> Option<u16>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let wait = std::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout(wait, socket.next()).await.ok()? {
            Some(Ok(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => {}
            _ => return None,
        }
    }
}

/// A client protocol connection of `signed` over `/api/rpc`, after
/// `rpc.hello`.
pub async fn rpc(
    port: u16,
    signed: &Signed,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let origin = format!("http://127.0.0.1:{port}");
    let protocols = format!("prometheus-rpc.v1, csrf.{}", signed.csrf);
    let mut ws = socket(
        port,
        "/api/rpc",
        &[("Cookie", &signed.cookie), ("Origin", &origin)],
        &protocols,
    )
    .await;
    let hello = json!({"jsonrpc": "2.0", "id": 0, "method": "rpc.hello", "params": {
        "client": "dashboard", "client_version": "test", "protocol": {"major": 1, "minor": 0},
        "capabilities": {"server_requests": true, "events": []}}});
    let answer = request_on(&mut ws, &hello).await;
    assert!(answer.get("result").is_some(), "{answer}");
    ws
}

/// Sends a request frame and returns its response (events in between are
/// skipped).
pub async fn request_on<S>(ws: &mut S, frame: &Value) -> Value
where
    S: futures_util::Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Debug,
{
    ws.send(Message::text(frame.to_string())).await.unwrap();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(30), ws.next())
            .await
            .expect("an answer within 30 s");
        let Some(Ok(Message::Text(text))) = next else {
            panic!("socket ended: {next:?}");
        };
        let value: Value = serde_json::from_str(text.as_str()).unwrap();
        if value.get("id") == frame.get("id") && value.get("method").is_none() {
            return value;
        }
    }
}

/// Follows a remote sign-in from `GET /login` through the identity
/// provider back to the dashboard; the signed-in client.
pub fn sign_in_remote(opened: &Opened) -> Signed {
    let done = remote_callback(opened);
    assert_eq!(done.status, 303, "{done:?}");
    let cookie = set_cookie(&done)
        .and_then(|c| c.split(';').next())
        .unwrap()
        .to_owned();
    let session = get(opened.port, "/api/session", &cookie);
    Signed {
        csrf: session.json()["csrf"].as_str().unwrap().to_owned(),
        cookie,
    }
}

/// Goes through the remote sign-in up to the dashboard's callback; its
/// answer.
pub fn remote_callback(opened: &Opened) -> Answer {
    let start = Request::new("GET", "/login").send(opened.port);
    assert_eq!(start.status, 303, "{start:?}");
    let to_provider = location(&start);
    let provider = url::Url::parse(&to_provider).unwrap();
    let answer = Request::new("GET", &path_of(&to_provider))
        .header(
            "Host",
            &format!(
                "{}:{}",
                provider.host_str().unwrap(),
                provider.port().unwrap()
            ),
        )
        .send(provider.port().unwrap());
    assert_eq!(answer.status, 302, "{answer:?}");
    let back = location(&answer);
    Request::new("GET", &path_of(&back)).send(opened.port)
}

fn location(answer: &Answer) -> String {
    answer
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone())
        .unwrap()
}

/// A self-signed certificate for 127.0.0.1 written as `cert.pem` and
/// `key.pem` in `dir`.
pub fn certificate(dir: &Path) -> rustls::pki_types::CertificateDer<'static> {
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    std::fs::write(dir.join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.join("key.pem"), key.serialize_pem()).unwrap();
    cert.der().clone()
}

/// One raw HTTPS request to 127.0.0.1:`port` trusting `root`; the status
/// line's code and the raw answer.
pub async fn https(
    port: u16,
    root: &rustls::pki_types::CertificateDer<'static>,
    request: &str,
) -> (u16, String) {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root.clone()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls::pki_types::ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
    let mut tls = connector.connect(name, tcp).await.unwrap();
    tls.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let code = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, text)
}

pub const WEB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web");

/// Runs `web/e2e/<spec>` against `home`'s running daemon.
pub fn playwright(home: &Path, spec: &str, extra: &[(&str, &str)]) {
    let web = std::path::Path::new(WEB);
    let cli = web.join("node_modules/@playwright/test/cli.js");
    assert!(
        cli.is_file(),
        "the dashboard's browser tests need `npm ci` in web/ (scripts/check runs it)"
    );
    let out = std::process::Command::new("node")
        .current_dir(web)
        .arg(&cli)
        .args([
            "test",
            &format!("e2e/{spec}"),
            "--config",
            "playwright.config.ts",
        ])
        .env("PMT_BIN", super::BIN)
        .env("PMT_HOME", home)
        .env("PMT_USER_HOME", super::user_home(home))
        .env("PMT_PW_OUT", home.join("playwright"))
        .envs(extra.iter().copied())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{spec} failed\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

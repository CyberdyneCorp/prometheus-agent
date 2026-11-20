//! The managed scheduler contract inside the real daemon, against a mock
//! managed scheduler (spec: scheduled-jobs, Managed scheduler contract):
//! jobs are armed with `dedup_key = "<job_id>:<fire_at>"`, fires arrive on
//! `POST /api/cron/fire` with an asymmetrically signed JWT, a duplicate
//! fire runs the occurrence once, a symmetric-key token is refused and a
//! fire during the emergency stop is told to retry.

#![cfg(feature = "gateway")]

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use self::common::daemon::bound_port;
use self::common::gateway::{Daemon, wait};
use self::common::{mock_home, ok, run};

const AUDIENCE: &str = "prometheus-managed-test";

fn key() -> EcdsaKeyPair {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap()
}

fn jwks(key: &EcdsaKeyPair) -> Value {
    let point = key.public_key().as_ref();
    json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "fire-1", "alg": "ES256",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&point[33..65])}]})
}

fn claims() -> Value {
    let now = jiff::Timestamp::now().as_second();
    json!({"aud": AUDIENCE, "purpose": "cron_fire", "iat": now, "nbf": now, "exp": now + 120})
}

fn es256(key: &EcdsaKeyPair, claims: &Value) -> String {
    let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","kid":"fire-1","typ":"JWT"}"#);
    let message = format!("{head}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let sig = key.sign(&SystemRandom::new(), message.as_bytes()).unwrap();
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
}

fn hs256(claims: &Value) -> String {
    let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"fire-1","typ":"JWT"}"#);
    let message = format!("{head}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let mac = ring::hmac::sign(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"shared-secret"),
        message.as_bytes(),
    );
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(mac.as_ref()))
}

/// One `POST /api/cron/fire`; the status, the `Retry-After` header and
/// the JSON body.
fn fire(port: u16, token: &str, body: &Value) -> (u16, Option<String>, Value) {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let request = format!(
        "POST /api/cron/fire HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {token}\r\n\
         content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or_default();
    let retry = head.lines().find_map(|l| {
        l.to_ascii_lowercase()
            .strip_prefix("retry-after: ")
            .map(str::to_owned)
    });
    (
        status,
        retry,
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

fn runs(home: &Path, job: &str) -> usize {
    let doc: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "cron", "runs", job]))).unwrap();
    doc["runs"].as_array().map_or(0, Vec::len)
}

/// Scenarios: Duplicate external fire; Symmetric-key token rejected.
#[test]
fn managed_fires_are_verified_deduplicated_and_held_by_the_emergency_stop() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let scheduler = runtime.block_on(MockServer::start());
    let signer = key();
    runtime.block_on(
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks(&signer)))
            .mount(&scheduler),
    );
    runtime.block_on(
        Mock::given(method("POST"))
            .and(path("/arm"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&scheduler),
    );
    let home = mock_home();
    let home = home.path();
    common::append_config(
        home,
        &format!(
            "\n[cron]\ntimezone = \"UTC\"\nprovider = \"managed\"\ntick_interval_secs = 10\n\n\
             [cron.managed]\nendpoint = \"{0}/arm\"\njwks_url = \"{0}/jwks\"\naudience = \"{AUDIENCE}\"\n\
             callback_url = \"http://127.0.0.1/api/cron/fire\"\n",
            scheduler.uri()
        ),
    );
    std::fs::write(
        home.join("daemon.toml"),
        "[gateway.http]\nlisten = \"127.0.0.1:0\"\n",
    )
    .unwrap();
    let doc: Value = serde_json::from_str(&ok(&run(
        home,
        &[
            "--json", "cron", "add", "every 1h", "--prompt", "digest", "--yes",
        ],
    )))
    .unwrap();
    let job = doc["job"]["id"].as_str().unwrap().to_owned();
    let next: jiff::Timestamp = doc["job"]["next_run_at"].as_str().unwrap().parse().unwrap();
    let fire_at = next.to_string();
    let dedup_key = format!("{job}:{fire_at}");

    // The CLI armed the new job at its next run.
    let armed = runtime.block_on(scheduler.received_requests()).unwrap();
    let arm: Value = serde_json::from_slice(&armed.last().expect("an arm request").body).unwrap();
    assert_eq!(arm["dedup_key"], dedup_key);
    assert_eq!(arm["job_id"], job);
    assert_eq!(arm["fire_at"], fire_at);

    let daemon = Daemon::start(home);
    let port = bound_port(home, "listener:gateway.http");
    let body = json!({"job_id": job, "fire_at": fire_at, "dedup_key": dedup_key});

    let (status, _, refused) = fire(port, &hs256(&claims()), &body);
    assert_eq!(
        (status, refused),
        (401, json!({"error": "invalid fire token"}))
    );
    let mut wrong_audience = claims();
    wrong_audience["aud"] = json!("someone-else");
    assert_eq!(fire(port, &es256(&signer, &wrong_audience), &body).0, 401);

    ok(&run(home, &["pause", "--reason", "maintenance"]));
    let (status, retry, _) = fire(port, &es256(&signer, &claims()), &body);
    assert_eq!((status, retry.as_deref()), (503, Some("60")));
    ok(&run(home, &["pause", "off"]));

    let token = es256(&signer, &claims());
    let (first, _, started) = fire(port, &token, &body);
    assert_eq!(started["status"], "started", "{started}");
    let (second, _, again) = fire(port, &token, &body);
    assert_eq!((first, second), (200, 200), "{again}");
    assert_eq!(again["status"], "already_handled");
    wait("the fired run is recorded", Duration::from_secs(30), || {
        runs(home, &job) == 1
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(runs(home, &job), 1, "the occurrence ran once");

    // The next occurrence is armed after the fire.
    let next_key = format!(
        "{job}:{}",
        next.checked_add(jiff::SignedDuration::from_hours(1))
            .unwrap()
    );
    wait(
        "the next occurrence is armed",
        Duration::from_secs(30),
        || {
            runtime
                .block_on(scheduler.received_requests())
                .unwrap()
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains(&next_key))
        },
    );
    daemon.stop(home);
}

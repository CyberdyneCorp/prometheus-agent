//! A fake OpenID Connect provider for the dashboard's remote sign-in
//! tests: discovery, an authorization endpoint that signs `subject` in at
//! once (redirecting back with a code), a token endpoint answering an
//! ES256 ID token for the code's nonce, and its JWKS. As the account
//! portal it also answers who owns an OAuth client
//! (`/api/oauth/clients/<id>`: `subject` owns [`OWNED_CLIENT`] only).
//! Plain HTTP on loopback, served by a thread until the process ends.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde_json::{Value, json};

/// The client id the fake provider issues tokens for.
pub const CLIENT_ID: &str = "prometheus-dashboard";
/// The one portal client the signed-in account owns.
pub const OWNED_CLIENT: &str = "cl_owned";

struct Provider {
    issuer: String,
    subject: String,
    key: EcdsaKeyPair,
    /// Nonce of each issued code.
    codes: Mutex<HashMap<String, String>>,
}

/// Starts the provider signing everyone in as `subject`; its issuer URL.
pub fn start(subject: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let key =
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let provider = Arc::new(Provider {
        issuer: issuer.clone(),
        subject: subject.to_owned(),
        key,
        codes: Mutex::default(),
    });
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let provider = Arc::clone(&provider);
            std::thread::spawn(move || serve(&provider, stream));
        }
    });
    issuer
}

fn query(target: &str) -> HashMap<String, String> {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

fn serve(provider: &Provider, stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let mut length = 0;
    let mut bearer = String::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                value
                    .trim()
                    .trim_start_matches("Bearer ")
                    .clone_into(&mut bearer);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let path = target.split('?').next().unwrap_or("/");
    let answer = match path {
        "/.well-known/openid-configuration" => ok(&json!({
            "issuer": provider.issuer,
            "authorization_endpoint": format!("{}/authorize", provider.issuer),
            "token_endpoint": format!("{}/token", provider.issuer),
            "jwks_uri": format!("{}/jwks", provider.issuer),
        })),
        "/authorize" => authorize(provider, &query(&target)),
        "/token" => token(provider, &body),
        "/jwks" => ok(&jwks(&provider.key)),
        p if p.starts_with("/api/oauth/clients/") => {
            let owned = p.ends_with(OWNED_CLIENT) && bearer == access_token(&provider.subject);
            if owned {
                ok(&json!({"client_id": OWNED_CLIENT, "owner": provider.subject}))
            } else {
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_owned()
            }
        }
        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    };
    let mut stream = stream;
    let _ = stream.write_all(answer.as_bytes());
}

fn ok(body: &Value) -> String {
    let text = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
}

/// Signs the subject in at once: back to the redirect URI with a code.
fn authorize(provider: &Provider, params: &HashMap<String, String>) -> String {
    let code = format!("code-{}", provider.codes.lock().unwrap().len());
    provider
        .codes
        .lock()
        .unwrap()
        .insert(code.clone(), params["nonce"].clone());
    let mut back = url::Url::parse(&params["redirect_uri"]).unwrap();
    back.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {back}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn token(provider: &Provider, body: &[u8]) -> String {
    let form: HashMap<String, String> = url::form_urlencoded::parse(body).into_owned().collect();
    let Some(nonce) = provider.codes.lock().unwrap().remove(&form["code"]) else {
        return "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned();
    };
    let claims = json!({
        "iss": provider.issuer,
        "aud": form.get("client_id").map_or(CLIENT_ID, String::as_str),
        "sub": provider.subject,
        "exp": jiff::Timestamp::now().as_second() + 300,
        "nonce": nonce,
    });
    ok(
        &json!({"id_token": sign(&provider.key, &claims), "token_type": "Bearer",
               "access_token": access_token(&provider.subject)}),
    )
}

fn access_token(subject: &str) -> String {
    format!("at-{subject}")
}

fn jwks(key: &EcdsaKeyPair) -> Value {
    let point = key.public_key().as_ref();
    json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "test", "alg": "ES256",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&point[33..65])}]})
}

fn sign(key: &EcdsaKeyPair, claims: &Value) -> String {
    let head = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256","kid":"test","typ":"JWT"}"#);
    let body = URL_SAFE_NO_PAD.encode(claims.to_string());
    let message = format!("{head}.{body}");
    let signature = key.sign(&SystemRandom::new(), message.as_bytes()).unwrap();
    format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
}

/// `daemon.toml` of a dashboard signing users in through `issuer`, with
/// `drivers` listed as drivers.
pub fn daemon_toml(issuer: &str, subjects: &[&str], drivers: &[&str]) -> String {
    format!(
        "[dashboard.auth]\nmode = \"oidc\"\ndrivers = {drivers:?}\n[dashboard.auth.oidc]\nissuer = {issuer:?}\nclient_id = {CLIENT_ID:?}\nallowed_subjects = {subjects:?}\n"
    )
}

/// `daemon.toml` of a dashboard signing users in through the account
/// portal at `issuer` with the registered client `client_id`.
pub fn portal_toml(issuer: &str, client_id: &str) -> String {
    format!(
        "[dashboard.auth]\nmode = \"portal\"\n[dashboard.auth.portal]\nissuer = {issuer:?}\nclient_id = {client_id:?}\n"
    )
}

//! The `webhook` connection mode: Feishu posts events to the adapter's
//! route on the shared listener. Each request is checked in this order:
//! the source's rate (120 per 60 s, else 429), the body (JSON, at most
//! 1 MB, else 413/415/400), the encrypted envelope (decrypted with the
//! encrypt key), the verification token (401 on mismatch), the URL
//! challenge (echoed only after the token passed), and the signature
//! `sha256(timestamp + nonce + encrypt_key + body)` (401 on mismatch).
//! A source that keeps failing (25 errors in a row within 6 hours) earns
//! a WARN, then another every 25.

use adapter_kit::graph::PEER_HEADER;
use adapter_kit::{HttpRequest, HttpResponse, clock, crypt, header, host, respond, sign};
use serde_json::{Value, json};

/// The largest body accepted.
pub const MAX_BODY: usize = 1024 * 1024;
const RATE_MAX: u32 = 120;
const RATE_WINDOW_MS: u64 = 60_000;
const ANOMALY_THRESHOLD: u32 = 25;
const ANOMALY_TTL_MS: u64 = 6 * 3600 * 1000;
const MAX_SOURCES: usize = 4096;

/// The verification secrets.
pub struct Keys {
    /// `FEISHU_VERIFICATION_TOKEN`.
    pub token: Option<String>,
    /// `FEISHU_ENCRYPT_KEY`.
    pub encrypt: Option<String>,
}

/// Per-source counters.
#[derive(Default)]
pub struct Guard {
    /// (source, requests in the window, window start).
    rates: Vec<(String, u32, u64)>,
    /// (source, consecutive errors, first error).
    anomalies: Vec<(String, u32, u64)>,
}

/// What a checked request carries.
pub enum Checked {
    /// An event to process.
    Event(Value),
    /// An answer to send as it is (the URL challenge).
    Answer(HttpResponse),
}

fn json_answer(status: u16, body: &Value) -> HttpResponse {
    respond(status, "application/json", body.to_string())
}

/// Decrypts an `{"encrypt": ..}` envelope.
fn decrypt(key: &str, envelope: &str) -> Result<Value, String> {
    let data = sign::base64_decode(envelope)?;
    if data.len() < 32 {
        return Err("short envelope".to_owned());
    }
    let key = sign::sha256(key.as_bytes());
    let plain = crypt::aes256_cbc_decrypt(&key, &data[..16], &data[16..])?;
    let plain = crypt::unpad(plain, 16)?;
    serde_json::from_slice(&plain).map_err(|e| e.to_string())
}

/// Whether the signature headers sign `body` with `key`.
fn signed(request: &HttpRequest, key: &str) -> bool {
    let field = |name: &str| header(request, name).unwrap_or_default();
    let (timestamp, nonce, signature) = (
        field("x-lark-request-timestamp"),
        field("x-lark-request-nonce"),
        field("x-lark-signature"),
    );
    if timestamp.is_empty() || nonce.is_empty() || signature.is_empty() {
        return false;
    }
    let mut message = format!("{timestamp}{nonce}{key}").into_bytes();
    message.extend_from_slice(&request.body);
    sign::same(
        sign::hex(&sign::sha256(&message)).as_bytes(),
        signature.as_bytes(),
    )
}

impl Guard {
    /// Whether `source` is within its rate.
    fn within_rate(&mut self, source: &str) -> bool {
        let now = clock::now_ms();
        if let Some(entry) = self.rates.iter_mut().find(|(s, ..)| s == source) {
            if now.saturating_sub(entry.2) >= RATE_WINDOW_MS {
                *entry = (source.to_owned(), 1, now);
                return true;
            }
            entry.1 += 1;
            return entry.1 <= RATE_MAX;
        }
        self.rates
            .retain(|(_, _, start)| now.saturating_sub(*start) < RATE_WINDOW_MS);
        if self.rates.len() >= MAX_SOURCES {
            // Full only under abuse: an untracked source is refused.
            return false;
        }
        self.rates.push((source.to_owned(), 1, now));
        true
    }

    fn failed(&mut self, source: &str, status: u16, why: &str) -> HttpResponse {
        let now = clock::now_ms();
        let at = match self.anomalies.iter().position(|(s, ..)| s == source) {
            Some(at) => at,
            None => {
                if self.anomalies.len() >= MAX_SOURCES {
                    self.anomalies.remove(0);
                }
                self.anomalies.push((source.to_owned(), 0, now));
                self.anomalies.len() - 1
            }
        };
        let entry = &mut self.anomalies[at];
        if now.saturating_sub(entry.2) > ANOMALY_TTL_MS {
            *entry = (source.to_owned(), 0, now);
        }
        entry.1 += 1;
        if entry.1.is_multiple_of(ANOMALY_THRESHOLD) {
            host::warn(&format!(
                "Feishu webhook: {} failed requests in a row from {source} (last: {status} {why})",
                entry.1
            ));
        }
        json_answer(status, &json!({"code": status, "msg": why}))
    }

    /// Checks one request.
    ///
    /// # Errors
    ///
    /// The answer refusing it.
    pub fn check(&mut self, request: &HttpRequest, keys: &Keys) -> Result<Checked, HttpResponse> {
        let source = header(request, PEER_HEADER).unwrap_or("unknown").to_owned();
        if !self.within_rate(&source) {
            return Err(self.failed(&source, 429, "too many requests"));
        }
        let checked = self.verify(request, keys);
        match checked {
            Ok(checked) => {
                self.anomalies.retain(|(s, ..)| *s != source);
                Ok(checked)
            }
            Err((status, why)) => Err(self.failed(&source, status, why)),
        }
    }

    fn verify(&self, request: &HttpRequest, keys: &Keys) -> Result<Checked, (u16, &'static str)> {
        if request.method != "POST" {
            return Err((405, "method not allowed"));
        }
        if request.body.len() > MAX_BODY {
            return Err((413, "request body too large"));
        }
        let kind = header(request, "content-type").unwrap_or_default();
        if !kind.is_empty() && !kind.to_ascii_lowercase().starts_with("application/json") {
            return Err((415, "unsupported media type"));
        }
        let mut payload: Value =
            serde_json::from_slice(&request.body).map_err(|_| (400, "invalid json"))?;
        if let Some(envelope) = payload["encrypt"].as_str() {
            let key = keys
                .encrypt
                .as_deref()
                .ok_or((400, "encrypted body without an encrypt key"))?;
            payload = decrypt(key, envelope).map_err(|_| (400, "cannot decrypt the body"))?;
        }
        if let Some(expected) = &keys.token {
            let got = payload["header"]["token"]
                .as_str()
                .or(payload["token"].as_str())
                .unwrap_or_default();
            if got.is_empty() || !sign::same(got.as_bytes(), expected.as_bytes()) {
                return Err((401, "invalid verification token"));
            }
        }
        if payload["type"] == "url_verification" {
            let challenge = json!({"challenge": payload["challenge"]});
            return Ok(Checked::Answer(json_answer(200, &challenge)));
        }
        if let Some(key) = &keys.encrypt
            && !signed(request, key)
        {
            return Err((401, "invalid signature"));
        }
        Ok(Checked::Event(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: &str, headers: &[(&str, &str)]) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/".into(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn the_signature_covers_timestamp_nonce_key_and_body() {
        let body = r#"{"schema":"2.0"}"#;
        let digest = sign::hex(&sign::sha256(format!("1n1k{body}").as_bytes()));
        let ok = request(
            body,
            &[
                ("x-lark-request-timestamp", "1"),
                ("x-lark-request-nonce", "n1"),
                ("x-lark-signature", &digest),
            ],
        );
        assert!(signed(&ok, "k"));
        assert!(!signed(&ok, "other"));
        assert!(!signed(&request(body, &[]), "k"));
    }

    #[test]
    fn an_encrypted_envelope_decrypts_with_the_hashed_key() {
        let key = sign::sha256(b"secret");
        let iv = [7u8; 16];
        let plain = crypt::pad(
            br#"{"type":"url_verification","challenge":"c"}"#.to_vec(),
            16,
        );
        let mut data = iv.to_vec();
        data.extend(crypt::aes256_cbc_encrypt(&key, &iv, &plain).unwrap());
        let value = decrypt("secret", &sign::base64(&data)).unwrap();
        assert_eq!(value["challenge"], "c");
        assert!(decrypt("wrong", &sign::base64(&data)).is_err());
    }
}

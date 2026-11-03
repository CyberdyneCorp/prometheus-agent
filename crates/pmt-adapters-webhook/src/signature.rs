//! Request signatures a webhook route accepts (spec: platform-adapters,
//! Webhook routes): GitHub `X-Hub-Signature-256` (`sha256=<hex HMAC of
//! the body>`), GitLab `X-Gitlab-Token` (the secret itself), Standard
//! Webhooks `webhook-id`/`webhook-timestamp`/`webhook-signature` (also
//! under Svix's `svix-*` names: base64 HMAC of `<id>.<ts>.<body>`, the key
//! the base64 after `whsec_`), and the generic `X-Webhook-Signature-V2`
//! (hex HMAC of `<ts>.<body>` with `X-Webhook-Timestamp`). Timestamped
//! schemes accept 300 seconds of skew either way. A request with none of
//! these headers is unsigned.

use base64::Engine as _;
use pmt_gateway::messaging::HttpRequest;
use pmt_gateway::messaging::sign::{constant_time_eq, hex, hmac_sha256};

/// Seconds a signed timestamp may be away from now.
pub(crate) const SKEW_SECS: i64 = 300;

fn fresh(timestamp: &str, now: i64) -> bool {
    timestamp
        .trim()
        .parse::<i64>()
        .is_ok_and(|ts| (now - ts).abs() <= SKEW_SECS)
}

/// The HMAC key of a Standard Webhooks secret (`whsec_<base64>`, or a raw
/// shared secret); `None` for an empty key.
fn standard_key(secret: &str) -> Option<Vec<u8>> {
    let key = match secret.strip_prefix("whsec_") {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?,
        None => secret.as_bytes().to_vec(),
    };
    (!key.iter().all(|b| *b == 0)).then_some(key)
}

fn standard(request: &HttpRequest, secret: &str, now: i64) -> Option<bool> {
    let names = if request.header("webhook-signature").is_some() {
        ["webhook-id", "webhook-timestamp", "webhook-signature"]
    } else {
        ["svix-id", "svix-timestamp", "svix-signature"]
    };
    let signatures = request.header(names[2])?;
    let (Some(id), Some(ts)) = (request.header(names[0]), request.header(names[1])) else {
        return Some(false);
    };
    let Some(key) = standard_key(secret).filter(|_| fresh(ts, now)) else {
        return Some(false);
    };
    let mut signed = format!("{id}.{ts}.").into_bytes();
    signed.extend_from_slice(&request.body);
    let expected = base64::engine::general_purpose::STANDARD.encode(hmac_sha256(&key, &signed));
    // Several space-separated `v1,<base64>` entries while a secret rotates.
    Some(signatures.split_whitespace().any(|entry| {
        entry.split_once(',').is_some_and(|(v, sig)| {
            v == "v1" && constant_time_eq(sig.as_bytes(), expected.as_bytes())
        })
    }))
}

/// Whether `request` carries a valid signature under `secret` at `now`
/// (Unix seconds).
#[must_use]
pub(crate) fn verify(request: &HttpRequest, secret: &str, now: i64) -> bool {
    if let Some(valid) = standard(request, secret, now) {
        return valid;
    }
    if let Some(given) = request.header("x-hub-signature-256") {
        let expected = format!(
            "sha256={}",
            hex(&hmac_sha256(secret.as_bytes(), &request.body))
        );
        return constant_time_eq(given.as_bytes(), expected.as_bytes());
    }
    if let Some(token) = request.header("x-gitlab-token") {
        return constant_time_eq(token.as_bytes(), secret.as_bytes());
    }
    if let Some(given) = request.header("x-webhook-signature-v2") {
        // The header commits to V2: no timestamp, no admission.
        let Some(ts) = request
            .header("x-webhook-timestamp")
            .filter(|ts| fresh(ts, now))
        else {
            return false;
        };
        let mut signed = format!("{}.", ts.trim()).into_bytes();
        signed.extend_from_slice(&request.body);
        let expected = hex(&hmac_sha256(secret.as_bytes(), &signed));
        return constant_time_eq(given.as_bytes(), expected.as_bytes());
    }
    false
}

/// GitHub's signature header of `body` (what a sender computes).
#[must_use]
pub fn github_signature(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hex(&hmac_sha256(secret.as_bytes(), body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn request(headers: &[(&str, &str)], body: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/ci".into(),
            query: None,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn github_signatures_cover_the_body() {
        let sig = github_signature("s3cret", b"{}");
        assert!(verify(
            &request(&[("x-hub-signature-256", &sig)], "{}"),
            "s3cret",
            NOW
        ));
        assert!(!verify(
            &request(&[("x-hub-signature-256", &sig)], "{\"x\":1}"),
            "s3cret",
            NOW
        ));
        assert!(!verify(&request(&[], "{}"), "s3cret", NOW), "unsigned");
    }

    #[test]
    fn gitlab_tokens_match_exactly() {
        assert!(verify(
            &request(&[("x-gitlab-token", "tok")], "{}"),
            "tok",
            NOW
        ));
        assert!(!verify(
            &request(&[("x-gitlab-token", "to")], "{}"),
            "tok",
            NOW
        ));
    }

    #[test]
    fn standard_webhooks_sign_id_timestamp_and_body_within_the_window() {
        let key = b"0123456789abcdef";
        let secret = format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode(key)
        );
        let ts = NOW.to_string();
        let signed = format!("msg_1.{ts}.{{}}");
        let sig =
            base64::engine::general_purpose::STANDARD.encode(hmac_sha256(key, signed.as_bytes()));
        let entries = format!("v1,bogus v1,{sig}");
        let headers = [
            ("webhook-id", "msg_1"),
            ("webhook-timestamp", ts.as_str()),
            ("webhook-signature", entries.as_str()),
        ];
        assert!(verify(&request(&headers, "{}"), &secret, NOW));
        assert!(
            !verify(&request(&headers, "{}"), &secret, NOW + SKEW_SECS + 1),
            "stale"
        );
        let svix = [
            ("svix-id", "msg_1"),
            ("svix-timestamp", ts.as_str()),
            ("svix-signature", entries.as_str()),
        ];
        assert!(verify(&request(&svix, "{}"), &secret, NOW));
    }

    #[test]
    fn generic_v2_needs_a_fresh_timestamp() {
        let ts = NOW.to_string();
        let sig = hex(&hmac_sha256(b"k", format!("{ts}.{{}}").as_bytes()));
        let signed = [
            ("x-webhook-signature-v2", sig.as_str()),
            ("x-webhook-timestamp", ts.as_str()),
        ];
        assert!(verify(&request(&signed, "{}"), "k", NOW));
        assert!(
            !verify(&request(&signed[..1], "{}"), "k", NOW),
            "no timestamp"
        );
        assert!(!verify(&request(&signed, "{}"), "k", NOW - 301), "stale");
    }
}

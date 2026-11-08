//! AWS Signature Version 4 for Bedrock requests: the canonical request,
//! the string to sign and the derived signing key, as AWS documents them
//! for every service but S3 (path segments are escaped twice).

use jiff::Timestamp;
use percent_encoding::utf8_percent_encode;
use ring::hmac;
use sha2::{Digest, Sha256};

use super::{Keys, PATH_SEGMENT};
use crate::request::hex;

/// One request to sign.
#[derive(Debug, Clone, Copy)]
pub struct Signing<'a> {
    /// `GET`, `POST`.
    pub method: &'a str,
    /// The full URL as it is sent.
    pub url: &'a reqwest::Url,
    /// Headers sent besides `host` and the ones signing adds (names in
    /// any case).
    pub headers: &'a [(&'a str, &'a str)],
    /// The body as sent.
    pub body: &'a [u8],
    /// The region (`us-east-1`).
    pub region: &'a str,
    /// The service (`bedrock`).
    pub service: &'a str,
    /// The signing time.
    pub now: Timestamp,
}

/// The headers to add to the request: `x-amz-date`, the session token
/// of temporary credentials, and `authorization`.
#[must_use]
pub fn sign(signing: &Signing<'_>, keys: &Keys) -> Vec<(String, String)> {
    let amz_date = signing.now.strftime("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let mut added = vec![("x-amz-date".to_owned(), amz_date.clone())];
    if let Some(token) = &keys.session_token {
        added.push(("x-amz-security-token".to_owned(), token.expose().to_owned()));
    }
    let mut headers: Vec<(String, String)> = signing
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .chain(std::iter::once(("host".to_owned(), host(signing.url))))
        .chain(added.iter().cloned())
        .collect();
    headers.sort();
    let signed: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    let signed = signed.join(";");
    let mut canonical_headers = String::new();
    for (name, value) in &headers {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value);
        canonical_headers.push('\n');
    }
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        signing.method,
        canonical_uri(signing.url),
        canonical_query(signing.url),
        hex(&Sha256::digest(signing.body)),
    );
    let scope = format!("{date}/{}/{}/aws4_request", signing.region, signing.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = mac(
        format!("AWS4{}", keys.secret_access_key.expose()).as_bytes(),
        date,
    );
    for part in [signing.region, signing.service, "aws4_request"] {
        key = mac(&key, part);
    }
    let signature = hex(&mac(&key, &to_sign));
    added.push((
        "authorization".to_owned(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            keys.access_key_id
        ),
    ));
    added
}

fn mac(key: &[u8], data: &str) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, data.as_bytes()).as_ref().to_vec()
}

/// `host[:port]` as the request sends it.
fn host(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// The URL's (already escaped) path segments, escaped once more.
fn canonical_uri(url: &reqwest::Url) -> String {
    let path = url.path();
    if path.is_empty() {
        return "/".to_owned();
    }
    path.split('/')
        .map(|segment| utf8_percent_encode(segment, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Query parameters sorted by name then value, each escaped.
fn canonical_query(url: &reqwest::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| {
            (
                utf8_percent_encode(&k, PATH_SEGMENT).to_string(),
                utf8_percent_encode(&v, PATH_SEGMENT).to_string(),
            )
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use pmt_core::secret::SecretString;

    use super::*;

    /// AWS's published example (IAM `ListUsers`, "Create a signed AWS API
    /// request" in the IAM user guide).
    #[test]
    fn the_documented_example_signs_identically() {
        let url =
            reqwest::Url::parse("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08")
                .unwrap();
        let keys = Keys {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: SecretString::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
            session_token: None,
            expires_at: None,
        };
        let headers = sign(
            &Signing {
                method: "GET",
                url: &url,
                headers: &[(
                    "Content-Type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                )],
                body: b"",
                region: "us-east-1",
                service: "iam",
                now: "2015-08-30T12:36:00Z".parse().unwrap(),
            },
            &keys,
        );
        assert_eq!(headers[0], ("x-amz-date".into(), "20150830T123600Z".into()));
        assert_eq!(
            headers[1].1,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn model_ids_are_escaped_twice_and_tokens_signed() {
        let url = reqwest::Url::parse(
            "https://bedrock-runtime.eu-west-1.amazonaws.com/model/eu.anthropic.claude-v1%3A0/converse-stream",
        )
        .unwrap();
        assert_eq!(
            canonical_uri(&url),
            "/model/eu.anthropic.claude-v1%253A0/converse-stream"
        );
        let keys = Keys {
            access_key_id: "AKID".into(),
            secret_access_key: SecretString::new("secret"),
            session_token: Some(SecretString::new("session")),
            expires_at: None,
        };
        let headers = sign(
            &Signing {
                method: "POST",
                url: &url,
                headers: &[],
                body: b"{}",
                region: "eu-west-1",
                service: "bedrock",
                now: Timestamp::UNIX_EPOCH,
            },
            &keys,
        );
        assert_eq!(
            headers[1],
            ("x-amz-security-token".into(), "session".into())
        );
        assert!(
            headers[2]
                .1
                .contains("SignedHeaders=host;x-amz-date;x-amz-security-token")
        );
    }
}

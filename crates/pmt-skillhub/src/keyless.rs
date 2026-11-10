//! Sigstore keyless signatures (spec: skills, Skill signatures and
//! provenance): a short-lived Fulcio certificate binds an OpenID identity
//! to a fresh key, the key signs the skill digest, and the Rekor
//! transparency log records it with a signed entry timestamp.
//!
//! Verification needs no network: the bundle carries the certificate and
//! the log entry, checked against the embedded Sigstore trust root
//! (`keyless/trusted_root.json`, the public-good instance): the
//! certificate chains to a Fulcio CA, the log entry's signed timestamp
//! verifies with a Rekor key and falls inside the certificate's validity,
//! the entry records this signature over this digest, and the signature
//! verifies with the certificate's key. Entries without a signed entry
//! timestamp (Rekor v2 checkpoints) are not accepted.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use pmt_core::error::{Error, codes};
use ring::signature::{self, KeyPair as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use x509_parser::prelude::{FromDer as _, GeneralName, X509Certificate};
use x509_parser::x509::SubjectPublicKeyInfo;

use crate::http::{Http, Request};

/// The bundle media type written.
pub const MEDIA_TYPE: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
/// The public-good Fulcio.
pub const FULCIO_URL: &str = "https://fulcio.sigstore.dev";
/// The public-good Rekor (v1, with signed entry timestamps).
pub const REKOR_URL: &str = "https://rekor.sigstore.dev";

const TRUSTED_ROOT: &str = include_str!("../keyless/trusted_root.json");
/// The Fulcio OIDC issuer extensions (v2 DER string, then v1 raw).
const ISSUER_V2: &str = "1.3.6.1.4.1.57264.1.8";
const ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";
/// The DER prefix of a P-256 `SubjectPublicKeyInfo` before the point.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// Base64 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawBytes {
    /// Base64 (standard) DER.
    pub raw_bytes: String,
}

/// A log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlogEntry {
    /// Its index in the log (decimal string).
    pub log_index: String,
    /// The log's key id.
    pub log_id: LogId,
    /// Entry kind.
    pub kind_version: KindVersion,
    /// When the log integrated it (Unix seconds, decimal string).
    pub integrated_time: String,
    /// The signed entry timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inclusion_promise: Option<Promise>,
    /// The entry body as logged (base64).
    pub canonicalized_body: String,
}

/// A log id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogId {
    /// Base64 key id (SHA-256 of the log's public key).
    pub key_id: String,
}

/// An entry kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindVersion {
    /// `hashedrekord`.
    pub kind: String,
    /// `0.0.1`.
    pub version: String,
}

/// A signed entry timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Promise {
    /// Base64 DER signature.
    pub signed_entry_timestamp: String,
}

/// The verification material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Material {
    /// The leaf certificate.
    pub certificate: RawBytes,
    /// The log entries.
    pub tlog_entries: Vec<TlogEntry>,
}

/// The signed message's digest and signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSignature {
    /// The message digest.
    pub message_digest: MessageDigest,
    /// Base64 DER ECDSA signature.
    pub signature: String,
}

/// A message digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDigest {
    /// `SHA2_256`.
    pub algorithm: String,
    /// Base64 digest.
    pub digest: String,
}

/// A Sigstore bundle (the subset a keyless message signature uses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bundle {
    /// [`MEDIA_TYPE`].
    pub media_type: String,
    /// Certificate and log entries.
    pub verification_material: Material,
    /// The signature.
    pub message_signature: MessageSignature,
}

/// A Fulcio CA: its chain, leaf issuer first, root last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    /// DER certificates.
    pub chain: Vec<Vec<u8>>,
}

/// A transparency log key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogKey {
    /// The key id (SHA-256 of the SPKI DER).
    pub key_id: Vec<u8>,
    /// The SPKI DER.
    pub spki: Vec<u8>,
}

/// The trust roots keyless signatures are checked against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustRoots {
    /// Fulcio CAs.
    pub authorities: Vec<Authority>,
    /// Rekor keys.
    pub logs: Vec<LogKey>,
}

fn b64(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text.trim()).ok()
}

impl TrustRoots {
    /// The embedded public-good trust root.
    #[must_use]
    pub fn public() -> Self {
        Self::parse(TRUSTED_ROOT).unwrap_or_default()
    }

    /// Reads a Sigstore `trusted_root.json`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let root: Value = serde_json::from_str(text).ok()?;
        let authorities = root["certificateAuthorities"]
            .as_array()?
            .iter()
            .map(|ca| Authority {
                chain: ca["certChain"]["certificates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| b64(c["rawBytes"].as_str()?))
                    .collect(),
            })
            .collect();
        let logs = root["tlogs"]
            .as_array()?
            .iter()
            .filter_map(|log| {
                Some(LogKey {
                    key_id: b64(log["logId"]["keyId"].as_str()?)?,
                    spki: b64(log["publicKey"]["rawBytes"].as_str()?)?,
                })
            })
            .collect();
        Some(Self { authorities, logs })
    }
}

/// Verifies `bundle` over `message`; the signer's identity
/// (`<subject> via <issuer>`).
///
/// # Errors
///
/// Why it does not verify.
pub fn verify(bundle: &Bundle, message: &[u8], roots: &TrustRoots) -> Result<String, String> {
    let leaf_der = b64(&bundle.verification_material.certificate.raw_bytes)
        .ok_or("the certificate is not base64")?;
    let (_, leaf) =
        X509Certificate::from_der(&leaf_der).map_err(|e| format!("bad certificate: {e}"))?;
    let entry = bundle
        .verification_material
        .tlog_entries
        .first()
        .ok_or("no transparency log entry")?;
    let integrated: i64 = entry
        .integrated_time
        .parse()
        .map_err(|_| "bad integrated time".to_owned())?;
    check_chain(&leaf, roots, integrated)?;
    check_promise(entry, roots)?;
    let signature =
        b64(&bundle.message_signature.signature).ok_or("the signature is not base64")?;
    let digest = Sha256::digest(message);
    if b64(&bundle.message_signature.message_digest.digest).as_deref() != Some(digest.as_slice()) {
        return Err("the bundle's message digest does not match the content".to_owned());
    }
    check_body(entry, &signature, &leaf_der, &digest)?;
    let point = leaf.public_key().subject_public_key.data.to_vec();
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, point)
        .verify(message, &signature)
        .map_err(|_| "the signature does not verify with the certificate's key".to_owned())?;
    identity(&leaf)
}

fn within(cert: &X509Certificate<'_>, at: i64) -> bool {
    let validity = cert.validity();
    validity.not_before.timestamp() <= at && at <= validity.not_after.timestamp()
}

/// The leaf chains to a Fulcio CA and was valid when logged.
fn check_chain(leaf: &X509Certificate<'_>, roots: &TrustRoots, at: i64) -> Result<(), String> {
    if !within(leaf, at) {
        return Err("the certificate was not valid when the signature was logged".to_owned());
    }
    let chained = roots
        .authorities
        .iter()
        .any(|ca| chains(leaf, &ca.chain, at));
    if chained {
        Ok(())
    } else {
        Err("the certificate does not chain to a Sigstore certificate authority".to_owned())
    }
}

fn chains(leaf: &X509Certificate<'_>, chain: &[Vec<u8>], at: i64) -> bool {
    let parsed: Vec<X509Certificate<'_>> = chain
        .iter()
        .filter_map(|der| X509Certificate::from_der(der).ok().map(|(_, c)| c))
        .collect();
    if parsed.is_empty() || parsed.len() != chain.len() {
        return false;
    }
    let mut child = leaf;
    for issuer in &parsed {
        if !within(issuer, at) || child.verify_signature(Some(issuer.public_key())).is_err() {
            return false;
        }
        child = issuer;
    }
    child.verify_signature(None).is_ok()
}

/// The signed entry timestamp verifies with the log's key.
fn check_promise(entry: &TlogEntry, roots: &TrustRoots) -> Result<(), String> {
    let promise = entry
        .inclusion_promise
        .as_ref()
        .ok_or("the log entry has no signed entry timestamp")?;
    let key_id = b64(&entry.log_id.key_id).ok_or("bad log id")?;
    let log = roots
        .logs
        .iter()
        .find(|l| l.key_id == key_id)
        .ok_or("the entry is from an unknown transparency log")?;
    let payload = set_payload(entry, &key_id)?;
    let set = b64(&promise.signed_entry_timestamp).ok_or("bad signed entry timestamp")?;
    let (_, spki) =
        SubjectPublicKeyInfo::from_der(&log.spki).map_err(|e| format!("bad log key: {e}"))?;
    let point = spki.subject_public_key.data.to_vec();
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, point)
        .verify(payload.as_bytes(), &set)
        .map_err(|_| "the signed entry timestamp does not verify".to_owned())
}

/// The canonical JSON a signed entry timestamp signs.
fn set_payload(entry: &TlogEntry, key_id: &[u8]) -> Result<String, String> {
    let integrated: i64 = entry
        .integrated_time
        .parse()
        .map_err(|_| "bad integrated time")?;
    let index: i64 = entry.log_index.parse().map_err(|_| "bad log index")?;
    Ok(format!(
        "{{\"body\":{},\"integratedTime\":{integrated},\"logID\":\"{}\",\"logIndex\":{index}}}",
        serde_json::to_string(&entry.canonicalized_body).unwrap_or_default(),
        hex(key_id)
    ))
}

/// The logged entry records this signature, certificate and digest.
fn check_body(
    entry: &TlogEntry,
    signature: &[u8],
    leaf_der: &[u8],
    digest: &[u8],
) -> Result<(), String> {
    let body = b64(&entry.canonicalized_body).ok_or("bad log entry body")?;
    let body: Value = serde_json::from_slice(&body).map_err(|_| "bad log entry body")?;
    let spec = &body["spec"];
    let logged_digest = spec["data"]["hash"]["value"].as_str().unwrap_or_default();
    let logged_signature = spec["signature"]["content"].as_str().and_then(b64);
    let logged_cert = spec["signature"]["publicKey"]["content"]
        .as_str()
        .and_then(b64)
        .and_then(|pem| pem_der(&String::from_utf8_lossy(&pem)));
    let matches = body["kind"] == "hashedrekord"
        && logged_digest == hex(digest)
        && logged_signature.as_deref() == Some(signature)
        && logged_cert.as_deref() == Some(leaf_der);
    if matches {
        Ok(())
    } else {
        Err("the transparency log entry is for another signature".to_owned())
    }
}

/// `<subject> via <issuer>` of a Fulcio certificate.
fn identity(leaf: &X509Certificate<'_>) -> Result<String, String> {
    let san = leaf
        .subject_alternative_name()
        .ok()
        .flatten()
        .and_then(|ext| {
            ext.value.general_names.iter().find_map(|name| match name {
                GeneralName::RFC822Name(s) | GeneralName::URI(s) => Some((*s).to_owned()),
                _ => None,
            })
        })
        .ok_or("the certificate names no identity")?;
    let issuer = leaf
        .extensions()
        .iter()
        .find_map(|ext| match ext.oid.to_id_string().as_str() {
            ISSUER_V2 => der_utf8(ext.value),
            ISSUER_V1 => std::str::from_utf8(ext.value).ok().map(str::to_owned),
            _ => None,
        });
    Ok(match issuer {
        Some(issuer) => format!("{san} via {issuer}"),
        None => san,
    })
}

/// A DER UTF8String (short or one-byte long form length).
fn der_utf8(bytes: &[u8]) -> Option<String> {
    let (&tag, rest) = bytes.split_first()?;
    if tag != 0x0c {
        return None;
    }
    let (len, rest) = match rest.split_first()? {
        (&0x81, rest) => (usize::from(*rest.first()?), rest.get(1..)?),
        (&n, rest) if n < 0x80 => (usize::from(n), rest),
        _ => return None,
    };
    std::str::from_utf8(rest.get(..len)?)
        .ok()
        .map(str::to_owned)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// PEM text of `der` with `label`.
#[must_use]
pub fn pem(label: &str, der: &[u8]) -> String {
    let body = STANDARD.encode(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for chunk in body.as_bytes().chunks(64) {
        text.push_str(&String::from_utf8_lossy(chunk));
        text.push('\n');
    }
    let _ = writeln!(text, "-----END {label}-----");
    text
}

/// The DER of the first PEM block in `text`.
#[must_use]
pub fn pem_der(text: &str) -> Option<Vec<u8>> {
    let start = text.find("-----BEGIN ")?;
    let body_start = start + text[start..].find('\n')? + 1;
    let end = body_start + text[body_start..].find("-----END ")?;
    let body: String = text[body_start..end].split_whitespace().collect();
    STANDARD.decode(body).ok()
}

/// The identity claim Fulcio checks the proof of possession against:
/// `email` when the token has one, else `sub`.
fn token_subject(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    claims["email"]
        .as_str()
        .or_else(|| claims["sub"].as_str())
        .map(str::to_owned)
}

fn failed(why: impl Into<String>) -> Error {
    Error::new(&codes::SKILL_SIGNATURE, why)
}

/// Where keyless signing goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Services {
    /// Fulcio base URL.
    pub fulcio: String,
    /// Rekor base URL.
    pub rekor: String,
}

impl Default for Services {
    fn default() -> Self {
        Self {
            fulcio: FULCIO_URL.to_owned(),
            rekor: REKOR_URL.to_owned(),
        }
    }
}

/// Signs `message` keyless with the OpenID token `token`: a fresh P-256
/// key, a Fulcio certificate for it and a Rekor entry.
///
/// # Errors
///
/// `skill.signature` when a service refuses or answers badly.
pub fn sign(
    message: &[u8],
    token: &str,
    http: &dyn Http,
    services: &Services,
) -> Result<Bundle, Error> {
    let rng = ring::rand::SystemRandom::new();
    let alg = &signature::ECDSA_P256_SHA256_ASN1_SIGNING;
    let pkcs8 = signature::EcdsaKeyPair::generate_pkcs8(alg, &rng)
        .map_err(|_| failed("cannot create a key"))?;
    let key = signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng)
        .map_err(|_| failed("cannot load the key"))?;
    let subject =
        token_subject(token).ok_or_else(|| failed("the identity token names no subject"))?;
    let proof = key
        .sign(&rng, subject.as_bytes())
        .map_err(|_| failed("cannot sign the proof of possession"))?;
    let spki = [P256_SPKI_PREFIX.as_slice(), key.public_key().as_ref()].concat();
    let leaf_der = certificate(http, services, token, &spki, proof.as_ref())?;
    let signature = key
        .sign(&rng, message)
        .map_err(|_| failed("cannot sign the digest"))?;
    let digest = Sha256::digest(message);
    let entry = log_entry(http, services, signature.as_ref(), &leaf_der, &digest)?;
    Ok(Bundle {
        media_type: MEDIA_TYPE.to_owned(),
        verification_material: Material {
            certificate: RawBytes {
                raw_bytes: STANDARD.encode(&leaf_der),
            },
            tlog_entries: vec![entry],
        },
        message_signature: MessageSignature {
            message_digest: MessageDigest {
                algorithm: "SHA2_256".to_owned(),
                digest: STANDARD.encode(digest),
            },
            signature: STANDARD.encode(signature.as_ref()),
        },
    })
}

/// Asks Fulcio for a certificate; the leaf's DER.
fn certificate(
    http: &dyn Http,
    services: &Services,
    token: &str,
    spki: &[u8],
    proof: &[u8],
) -> Result<Vec<u8>, Error> {
    let body = json!({
        "credentials": {"oidcIdentityToken": token},
        "publicKeyRequest": {
            "publicKey": {"algorithm": "ECDSA", "content": pem("PUBLIC KEY", spki)},
            "proofOfPossession": STANDARD.encode(proof),
        },
    });
    let url = format!("{}/api/v2/signingCert", services.fulcio);
    let response = http
        .send(Request::get(&url).json("POST", &body))
        .map_err(|e| failed(format!("{url}: {e}")))?;
    let value = response.json().filter(|_| response.ok()).ok_or_else(|| {
        failed(format!(
            "{url} refused the certificate request ({})",
            response.status
        ))
    })?;
    let chain = value["signedCertificateEmbeddedSct"]["chain"]["certificates"]
        .as_array()
        .or_else(|| value["signedCertificateDetachedSct"]["chain"]["certificates"].as_array())
        .ok_or_else(|| failed(format!("{url}: no certificate chain in the answer")))?;
    chain
        .first()
        .and_then(Value::as_str)
        .and_then(pem_der)
        .ok_or_else(|| failed(format!("{url}: unreadable certificate")))
}

/// Records the signature in Rekor; the bundle's log entry.
fn log_entry(
    http: &dyn Http,
    services: &Services,
    signature: &[u8],
    leaf_der: &[u8],
    digest: &[u8],
) -> Result<TlogEntry, Error> {
    let body = json!({
        "apiVersion": "0.0.1",
        "kind": "hashedrekord",
        "spec": {
            "data": {"hash": {"algorithm": "sha256", "value": hex(digest)}},
            "signature": {
                "content": STANDARD.encode(signature),
                "publicKey": {"content": STANDARD.encode(pem("CERTIFICATE", leaf_der))},
            },
        },
    });
    let url = format!("{}/api/v1/log/entries", services.rekor);
    let response = http
        .send(Request::get(&url).json("POST", &body))
        .map_err(|e| failed(format!("{url}: {e}")))?;
    let value = response
        .json()
        .filter(|_| response.ok())
        .ok_or_else(|| failed(format!("{url} refused the log entry ({})", response.status)))?;
    let entry = value
        .as_object()
        .and_then(|m| m.values().next())
        .ok_or_else(|| failed(format!("{url}: empty answer")))?;
    let key_id = entry["logID"]
        .as_str()
        .and_then(unhex)
        .ok_or_else(|| failed("bad log id"))?;
    Ok(TlogEntry {
        log_index: entry["logIndex"].as_i64().unwrap_or_default().to_string(),
        log_id: LogId {
            key_id: STANDARD.encode(key_id),
        },
        kind_version: KindVersion {
            kind: "hashedrekord".to_owned(),
            version: "0.0.1".to_owned(),
        },
        integrated_time: entry["integratedTime"]
            .as_i64()
            .unwrap_or_default()
            .to_string(),
        inclusion_promise: entry["verification"]["signedEntryTimestamp"]
            .as_str()
            .map(|set| Promise {
                signed_entry_timestamp: set.to_owned(),
            }),
        canonicalized_body: entry["body"].as_str().unwrap_or_default().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_trust_root_has_fulcio_and_rekor() {
        let roots = TrustRoots::public();
        assert!(roots.authorities.iter().any(|a| a.chain.len() == 2));
        assert!(roots.logs.iter().any(|l| l.key_id.len() == 32));
    }

    #[test]
    fn pem_round_trips() {
        let der = vec![1, 2, 3, 250];
        assert_eq!(pem_der(&pem("CERTIFICATE", &der)).unwrap(), der);
        assert_eq!(der_utf8(&[0x0c, 2, b'h', b'i']).as_deref(), Some("hi"));
    }
}

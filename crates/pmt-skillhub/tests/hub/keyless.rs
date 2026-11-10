//! Sigstore keyless signing and verification against a local Fulcio and
//! Rekor with their own trust roots.

use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use pmt_skillhub::http::{Http, Request, Response};
use pmt_skillhub::keyless::{self, Authority, LogKey, Services, TrustRoots};
use pmt_skillhub::sign::{self, Signature, SignatureFile};
use rcgen::{BasicConstraints, CertificateParams, CustomExtension, IsCa, Issuer, KeyPair, SanType};
use ring::signature::{self as rsig, KeyPair as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

struct Sigstore {
    inter_params: CertificateParams,
    inter_key: KeyPair,
    chain: Vec<Vec<u8>>,
    rekor: rsig::EcdsaKeyPair,
    rekor_spki: Vec<u8>,
}

impl Sigstore {
    fn new() -> Self {
        let root_key = KeyPair::generate().unwrap();
        let mut root_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let root = root_params.self_signed(&root_key).unwrap();
        let inter_key = KeyPair::generate().unwrap();
        let mut inter_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        inter_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let inter = inter_params
            .signed_by(&inter_key, &Issuer::from_params(&root_params, &root_key))
            .unwrap();
        let rng = ring::rand::SystemRandom::new();
        let alg = &rsig::ECDSA_P256_SHA256_ASN1_SIGNING;
        let pkcs8 = rsig::EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
        let rekor = rsig::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap();
        let rekor_spki = [P256_SPKI_PREFIX.as_slice(), rekor.public_key().as_ref()].concat();
        Self {
            inter_params,
            inter_key,
            chain: vec![inter.der().to_vec(), root.der().to_vec()],
            rekor,
            rekor_spki,
        }
    }

    fn roots(&self) -> TrustRoots {
        TrustRoots {
            authorities: vec![Authority {
                chain: self.chain.clone(),
            }],
            logs: vec![LogKey {
                key_id: Sha256::digest(&self.rekor_spki).to_vec(),
                spki: self.rekor_spki.clone(),
            }],
        }
    }

    fn certificate(&self, body: &Value) -> Value {
        let pem = body["publicKeyRequest"]["publicKey"]["content"]
            .as_str()
            .unwrap();
        let spki = rcgen::SubjectPublicKeyInfo::from_der(&keyless::pem_der(pem).unwrap()).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = vec![SanType::Rfc822Name("dev@example.com".try_into().unwrap())];
        params.custom_extensions = vec![CustomExtension::from_oid_content(
            &[1, 3, 6, 1, 4, 1, 57264, 1, 1],
            b"https://accounts.example.com".to_vec(),
        )];
        let leaf = params
            .signed_by(
                &spki,
                &Issuer::from_params(&self.inter_params, &self.inter_key),
            )
            .unwrap();
        json!({"signedCertificateEmbeddedSct": {"chain": {"certificates": [leaf.pem()]}}})
    }

    fn log(&self, raw_body: &[u8]) -> Value {
        let body = STANDARD.encode(raw_body);
        let log_id = Sha256::digest(&self.rekor_spki)
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            });
        let payload = format!(
            "{{\"body\":{},\"integratedTime\":1800000000,\"logID\":\"{log_id}\",\"logIndex\":7}}",
            serde_json::to_string(&body).unwrap()
        );
        let rng = ring::rand::SystemRandom::new();
        let set = self.rekor.sign(&rng, payload.as_bytes()).unwrap();
        json!({"uuid1": {
            "body": body, "integratedTime": 1_800_000_000, "logID": log_id, "logIndex": 7,
            "verification": {"signedEntryTimestamp": STANDARD.encode(set.as_ref())}
        }})
    }
}

impl Http for Sigstore {
    fn send(&self, request: Request) -> Result<Response, String> {
        let raw = request.body.unwrap_or_default();
        let body: Value = serde_json::from_slice(&raw).unwrap();
        let answer = if request.url.ends_with("/api/v2/signingCert") {
            self.certificate(&body)
        } else if request.url.ends_with("/api/v1/log/entries") {
            self.log(&raw)
        } else {
            return Ok(Response {
                status: 404,
                body: Vec::new(),
            });
        };
        Ok(Response {
            status: 201,
            body: answer.to_string().into_bytes(),
        })
    }
}

fn token() -> String {
    let claims =
        URL_SAFE_NO_PAD.encode(json!({"email": "dev@example.com", "sub": "123"}).to_string());
    format!("e30.{claims}.sig")
}

#[test]
fn a_keyless_signature_verifies_offline_and_tampering_breaks_it() {
    let sigstore = Sigstore::new();
    let services = Services {
        fulcio: "https://fulcio.test".to_owned(),
        rekor: "https://rekor.test".to_owned(),
    };
    let digest = "sha256:abc";
    let bundle = keyless::sign(digest.as_bytes(), &token(), &sigstore, &services).unwrap();
    let roots = sigstore.roots();
    let identity = keyless::verify(&bundle, digest.as_bytes(), &roots).unwrap();
    assert_eq!(identity, "dev@example.com via https://accounts.example.com");
    assert!(keyless::verify(&bundle, b"sha256:other", &roots).is_err());
    assert!(keyless::verify(&bundle, digest.as_bytes(), &TrustRoots::public()).is_err());
    let file = SignatureFile {
        version: 1,
        digest: digest.to_owned(),
        minisign: None,
        sigstore: Some(bundle),
    };
    let text = sign::render(&file);
    assert_eq!(
        sign::verify(Some(text.as_bytes()), digest, &roots),
        Signature::Verified("sigstore:dev@example.com via https://accounts.example.com".to_owned())
    );
    assert!(matches!(
        sign::verify(Some(text.as_bytes()), "sha256:x", &roots),
        Signature::Invalid(_)
    ));
}

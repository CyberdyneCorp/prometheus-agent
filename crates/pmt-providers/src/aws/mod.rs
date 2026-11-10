//! The `bedrock` provider's AWS plumbing (spec: model-providers, Gemini
//! native and AWS Bedrock providers): the region and endpoints, the
//! credential chain, `SigV4` request signing, the event-stream framing of
//! `ConverseStream` responses, and the control-plane model catalog.

pub mod catalog;
pub mod chain;
pub(crate) mod eventstream;
mod profile;
pub mod sigv4;

use jiff::Timestamp;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
use pmt_core::secret::SecretString;
use serde_json::json;

use crate::descriptor::{BundledWire, WireNeed};
use crate::registry::Provider;

/// The region used when `providers.bedrock.region` is unset.
pub const DEFAULT_REGION: &str = "us-east-1";

/// The service name runtime requests are signed for.
pub const SERVICE: &str = "bedrock";

/// Characters `SigV4` leaves unescaped in a path segment or query value.
pub(crate) const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Whether `provider` speaks Bedrock's Converse wire.
#[must_use]
pub fn is_bedrock(provider: &Provider) -> bool {
    provider.descriptor().wire_need == WireNeed::Bundled(BundledWire::BedrockConverse)
}

/// `providers.<id>.region`, else [`DEFAULT_REGION`].
#[must_use]
pub fn region(provider: &Provider) -> String {
    provider
        .settings()
        .region
        .clone()
        .unwrap_or_else(|| DEFAULT_REGION.to_owned())
}

/// Whether `region` looks like an AWS region (`eu-west-1`,
/// `us-gov-west-1`).
#[must_use]
pub fn valid_region(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    let (Some(first), Some(last)) = (parts.first(), parts.last()) else {
        return false;
    };
    (3..=4).contains(&parts.len())
        && first.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
        && last.chars().all(|c| c.is_ascii_digit())
}

/// The runtime endpoint of a Bedrock provider without a configured
/// `base_url`: `https://bedrock-runtime.<region>.amazonaws.com`.
#[must_use]
pub fn runtime_endpoint(provider: &Provider) -> Option<String> {
    is_bedrock(provider)
        .then(|| format!("https://bedrock-runtime.{}.amazonaws.com", region(provider)))
}

/// The control-plane endpoint the model catalog is listed from:
/// `providers.<id>.models_url`, else `https://bedrock.<region>.amazonaws.com`.
#[must_use]
pub fn control_endpoint(provider: &Provider) -> String {
    provider
        .settings()
        .models_url
        .clone()
        .unwrap_or_else(|| format!("https://bedrock.{}.amazonaws.com", region(provider)))
}

/// A set of AWS credentials.
#[derive(Clone)]
pub struct Keys {
    /// The access key id (not secret on its own).
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: SecretString,
    /// The session token of temporary credentials.
    pub session_token: Option<SecretString>,
    /// When temporary credentials expire.
    pub expires_at: Option<Timestamp>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

impl Keys {
    /// The keys as one secret value: a route's credential holds a single
    /// secret, which the signer reads back with [`Keys::from_secret`].
    #[must_use]
    pub fn to_secret(&self) -> SecretString {
        SecretString::new(
            json!({
                "access_key_id": self.access_key_id,
                "secret_access_key": self.secret_access_key.expose(),
                "session_token": self.session_token.as_ref().map(SecretString::expose),
            })
            .to_string(),
        )
    }

    /// The keys [`Keys::to_secret`] packed.
    #[must_use]
    pub fn from_secret(secret: &SecretString) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(secret.expose()).ok()?;
        Some(Self {
            access_key_id: value["access_key_id"].as_str()?.to_owned(),
            secret_access_key: SecretString::new(value["secret_access_key"].as_str()?),
            session_token: value["session_token"].as_str().map(SecretString::new),
            expires_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_are_validated_by_shape() {
        for ok in ["us-east-1", "eu-west-1", "ap-southeast-2", "us-gov-west-1"] {
            assert!(valid_region(ok), "{ok}");
        }
        for bad in [
            "",
            "eu",
            "EU-west-1",
            "eu-west",
            "europe-west-1",
            "eu_west_1",
        ] {
            assert!(!valid_region(bad), "{bad}");
        }
    }

    #[test]
    fn keys_round_trip_through_one_secret() {
        let keys = Keys {
            access_key_id: "AKID".into(),
            secret_access_key: SecretString::new("s3cr3t"),
            session_token: Some(SecretString::new("tok")),
            expires_at: None,
        };
        let back = Keys::from_secret(&keys.to_secret()).unwrap();
        assert_eq!(back.access_key_id, "AKID");
        assert_eq!(back.secret_access_key.expose(), "s3cr3t");
        assert_eq!(back.session_token.unwrap().expose(), "tok");
        assert!(!format!("{keys:?}").contains("s3cr3t"));
    }
}

//! Secrets are not configuration (spec: configuration-and-profiles): a
//! value that looks like a credential is refused for every non-secret key,
//! so a pasted API key never lands in `config.toml`. Keys that need a
//! credential are typed [`crate::schema::SecretName`] (a `*_secret` key
//! holding the secret's name) and are the only exception.

use pmt_core::secret::CREDENTIAL_PREFIXES;

/// Whether `value` looks like a credential: a known token prefix followed
/// by more characters, or a PEM private key.
#[must_use]
pub fn looks_like_credential(value: &str) -> bool {
    let value = value.trim();
    let pem = value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----");
    pem || CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| value.len() > prefix.len() + 3 && value.starts_with(prefix))
}

/// The refusal for a credential-looking value in the non-secret key `key`
/// (spec message). The validator reports it as `config.credential_in_file`
/// (exit 78) for a file and `config.credential_value` (exit 2) for a
/// command-line argument.
#[must_use]
pub fn message(key: &str) -> String {
    format!("{key}: this looks like a credential; store it with: prometheus secrets set <NAME>")
}

/// How to fix a refused credential.
pub const HINT: &str =
    "then reference it by name in the key's `*_secret` field (for example key_secret = \"NAME\")";

#[cfg(test)]
mod tests {
    use super::{looks_like_credential, message};

    #[test]
    fn known_formats_are_credentials() {
        for value in [
            "sk-live-123456",
            "ghp_abcdefghijklmnop",
            "xoxb-1234-5678",
            "AKIAIOSFODNN7EXAMPLE",
            "-----BEGIN RSA PRIVATE KEY-----\nMII...",
        ] {
            assert!(looks_like_credential(value), "{value}");
        }
    }

    #[test]
    fn ordinary_values_are_not() {
        for value in [
            "https://api.example.com/v1",
            "mono",
            "sk-",
            "skip",
            "AKIA",
            "default",
        ] {
            assert!(!looks_like_credential(value), "{value}");
        }
    }

    #[test]
    fn refusal_uses_the_spec_message() {
        assert_eq!(
            message("providers.acme.base_url"),
            "providers.acme.base_url: this looks like a credential; store it with: prometheus secrets set <NAME>"
        );
    }
}

//! Secrets are not configuration (spec: configuration-and-profiles): a
//! value that looks like a credential is refused for every non-secret key,
//! so a pasted API key never lands in `config.toml`.

use pmt_core::error::{Error, codes};

/// Prefixes of common credential formats (OpenAI/Anthropic `sk-`, GitHub,
/// Slack, AWS access keys, GitLab, Google API keys).
const PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "ASIA",
    "glpat-",
    "AIza",
];

/// Whether `value` looks like a credential: a known token prefix followed
/// by more characters, or a PEM private key.
#[must_use]
pub fn looks_like_credential(value: &str) -> bool {
    let value = value.trim();
    let pem = value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----");
    pem || PREFIXES
        .iter()
        .any(|prefix| value.len() > prefix.len() + 3 && value.starts_with(prefix))
}

/// Refuses a credential-looking string for the non-secret key `key`.
///
/// # Errors
///
/// `config.credential_value` (exit 2) with the spec message.
pub fn refuse(key: &str, value: &str) -> Result<(), Error> {
    if looks_like_credential(value) {
        return Err(Error::new(
            &codes::CONFIG_CREDENTIAL_VALUE,
            format!(
                "{key}: this looks like a credential; store it with: prometheus secrets set <NAME>"
            ),
        )
        .with_hint("then point the key at the secret name (for example key_secret = \"NAME\")"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{looks_like_credential, refuse};

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
        let err = refuse("providers.acme.base_url", "sk-live-123456").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().ends_with(
            "this looks like a credential; store it with: prometheus secrets set <NAME>"
        ));
    }
}

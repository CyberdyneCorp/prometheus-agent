//! What a profile's logs and trajectories are redacted with: its known
//! secrets (unless `security.redact_secrets = false`) and, independently,
//! PII (`privacy.redact_pii`; spec: security-boundaries, Known-secret
//! redaction everywhere; Optional PII redaction).

use pmt_config::scope::ProfileScope;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;

/// The profile's output redactor; `None` when neither applies.
///
/// # Errors
///
/// Secret store errors; the PII key cannot be read or created.
pub fn profile_redactor(scope: &ProfileScope) -> Result<Option<Redactor>, Error> {
    let config = scope.config();
    let secrets = if config.security.redact_secrets {
        Some(scope.secrets().redactor()?)
    } else {
        None
    };
    let pii = pmt_config::privacy::pii(scope.dir(), &config.privacy)?;
    Ok(match (secrets, pii) {
        (secrets, Some(pii)) => Some(secrets.unwrap_or_default().with_pii(pii)),
        (secrets, None) => secrets,
    })
}

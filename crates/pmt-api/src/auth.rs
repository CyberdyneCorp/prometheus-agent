//! Profile-bound routing and bearer authentication (spec: api-server,
//! Profile-bound routing and bearer authentication). A request names its
//! profile with the `/p/<profile>/` prefix (none: `default`) and
//! authenticates with that profile's own key only, compared in constant
//! time; a profile without a usable key is not served at all.

use pmt_core::secret::SecretString;
use sha2::{Digest as _, Sha256};

use crate::ApiHost;
use crate::http::{Reply, Req, error, not_found};

/// The shortest key a profile is served with.
pub(crate) const MIN_KEY_CHARS: usize = 32;

/// An authenticated caller.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    /// The routed profile.
    pub profile: String,
    /// A fingerprint of the presented key (idempotency keys are scoped
    /// to it).
    pub key_fp: String,
}

/// `path` without its `/p/<profile>` prefix, and the profile it names
/// (`default` without one).
pub(crate) fn split(path: &str) -> (String, String) {
    if let Some(rest) = path.strip_prefix("/p/") {
        let (profile, below) = rest.split_once('/').unwrap_or((rest, ""));
        return (profile.to_owned(), format!("/{below}"));
    }
    ("default".to_owned(), path.to_owned())
}

/// The profile's key when it is served: the API server is enabled and
/// its `api_server.key_secret` holds at least [`MIN_KEY_CHARS`]
/// characters.
pub(crate) fn profile_key(host: &dyn ApiHost, profile: &str) -> Option<SecretString> {
    let scope = host.api_scope(profile)?;
    let config = &scope.config().api_server;
    if !config.enabled {
        return None;
    }
    host.api_secret(profile, &config.key_secret.0)
        .filter(|key| key.expose().chars().count() >= MIN_KEY_CHARS)
}

/// Authenticates `req` for `profile`: 404 when the profile is not served
/// here, 401 for a missing or wrong key.
pub(crate) fn authenticate(host: &dyn ApiHost, profile: &str, req: &Req) -> Result<Caller, Reply> {
    let Some(key) = profile_key(host, profile) else {
        return Err(not_found("the route"));
    };
    let presented = req
        .header("authorization")
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim)
        .unwrap_or_default();
    if !pmt_gateway::messaging::sign::constant_time_eq(
        presented.as_bytes(),
        key.expose().as_bytes(),
    ) {
        return Err(unauthorized());
    }
    Ok(Caller {
        profile: profile.to_owned(),
        key_fp: fingerprint(presented),
    })
}

fn unauthorized() -> Reply {
    crate::http::with_header(
        error(
            401,
            "authentication_error",
            "invalid_api_key",
            "invalid or missing API key",
        ),
        "www-authenticate",
        "Bearer",
    )
}

/// The first 16 hex digits of the key's SHA-256.
fn fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    pmt_gateway::messaging::sign::hex(&digest[..8])
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, split};

    #[test]
    fn the_prefix_names_the_profile() {
        assert_eq!(
            split("/p/alice/v1/models"),
            ("alice".to_owned(), "/v1/models".to_owned())
        );
        assert_eq!(
            split("/v1/models"),
            ("default".to_owned(), "/v1/models".to_owned())
        );
        assert_eq!(split("/p/bob"), ("bob".to_owned(), "/".to_owned()));
    }

    #[test]
    fn fingerprints_differ_per_key() {
        assert_eq!(fingerprint("a").len(), 16);
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }
}

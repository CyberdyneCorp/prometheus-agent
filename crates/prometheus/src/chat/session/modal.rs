//! How the first-party `modal` terminal backend authenticates (spec:
//! execution-environments, First-party backend plugins):
//! `terminal.modal.mode` `direct` uses the user's `MODAL_TOKEN_ID` and
//! `MODAL_TOKEN_SECRET` secrets, `managed` a subscription gateway
//! credential (billed as `subscription:<provider>`), and `auto` (default)
//! the first of the two that is available. `prometheus doctor terminal`
//! shows the chosen mode, or why neither is available.
//!
//! The plugin never sees either secret: it sends its requests through the
//! host credential handle `modal` ([`credential`]; spec: plugin-system,
//! Host credential handles), which attaches them for the resolved mode
//! and only to that mode's origin.

use pmt_config::ProfileScope;
use pmt_config::schema::ModalMode;
use pmt_core::secret::SecretString;
use pmt_plugins::host::ports::{Credential, CredentialLookup};
use pmt_providers::resolve::CredentialExtras;

/// The secrets of `direct` mode.
pub const SECRETS: [&str; 2] = ["MODAL_TOKEN_ID", "MODAL_TOKEN_SECRET"];

/// The provider whose sign-in is the subscription gateway credential.
pub const SUBSCRIPTION: &str = "nous";

/// Modal's API (`direct`): its gRPC control plane.
pub const DIRECT_ORIGIN: &str = "https://api.modal.com";

/// The subscription's Modal gateway (`managed`).
pub const GATEWAY_ORIGIN: &str = "https://modal-gateway.nousresearch.com";

/// The host credential handle the plugin names.
pub const HANDLE: &str = "modal";

/// The mode the backend runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The user's Modal account.
    Direct,
    /// The subscription gateway; usage is billed to the subscription.
    Managed {
        /// `subscription:<provider>`.
        billing_source: String,
    },
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => f.write_str("direct (MODAL_TOKEN_ID, MODAL_TOKEN_SECRET)"),
            Self::Managed { billing_source } => {
                write!(f, "managed (billing_source = {billing_source})")
            }
        }
    }
}

/// The mode for `setting` given whether both Modal secrets are stored and
/// whether the subscription is signed in.
///
/// # Errors
///
/// Why the setting cannot be met, naming every missing option.
pub fn resolve(setting: ModalMode, secrets: bool, subscription: bool) -> Result<Mode, String> {
    let direct = || {
        format!(
            "no Modal secrets for direct mode (store {} and {} with `prometheus secrets set`)",
            SECRETS[0], SECRETS[1]
        )
    };
    let managed = || {
        format!(
            "no subscription signed in for managed mode (`prometheus auth login {SUBSCRIPTION}`)"
        )
    };
    let billed = || Mode::Managed {
        billing_source: format!("subscription:{SUBSCRIPTION}"),
    };
    match setting {
        ModalMode::Direct | ModalMode::Auto if secrets => Ok(Mode::Direct),
        ModalMode::Direct => Err(direct()),
        ModalMode::Managed | ModalMode::Auto if subscription => Ok(billed()),
        ModalMode::Managed => Err(managed()),
        ModalMode::Auto => Err(format!("{}; {}", direct(), managed())),
    }
}

/// The mode of profile `scope`.
///
/// # Errors
///
/// As [`resolve`].
pub fn of_profile(scope: &ProfileScope) -> Result<Mode, String> {
    let store = scope.secrets().store();
    let secrets = SECRETS
        .iter()
        .all(|name| store.get(name).ok().flatten().is_some());
    let credentials = pmt_providers::auth::Credentials::new(store.clone());
    let subscription = CredentialExtras::subscription(&credentials, SUBSCRIPTION)
        .ok()
        .flatten()
        .is_some();
    resolve(scope.config().terminal.modal.mode, secrets, subscription)
}

/// The credential behind the `modal` handle for profile `scope`: the
/// token headers of `direct` mode for Modal's API, or the subscription
/// token for the gateway (billed as `subscription:nous`).
#[must_use]
pub fn credential(scope: &ProfileScope) -> CredentialLookup {
    let host = |origin: &str| origin.trim_start_matches("https://").to_owned();
    match of_profile(scope) {
        Err(why) => CredentialLookup::Unavailable(why),
        Ok(Mode::Direct) => {
            let secrets = scope.secrets();
            let mut headers = Vec::new();
            for (name, header) in SECRETS
                .iter()
                .zip(["x-modal-token-id", "x-modal-token-secret"])
            {
                match secrets.resolve(name) {
                    Ok(value) => headers.push((header.to_owned(), value)),
                    Err(_) => {
                        return CredentialLookup::Unavailable(format!(
                            "secret {name} is not readable"
                        ));
                    }
                }
            }
            CredentialLookup::Ready(Credential {
                kind: "direct".to_owned(),
                origin: DIRECT_ORIGIN.to_owned(),
                billing_source: None,
                hosts: vec![host(DIRECT_ORIGIN)],
                headers,
            })
        }
        Ok(Mode::Managed { billing_source }) => {
            let credentials =
                pmt_providers::auth::Credentials::new(scope.secrets().store().clone());
            let Some(signed_in) = CredentialExtras::subscription(&credentials, SUBSCRIPTION)
                .ok()
                .flatten()
            else {
                return CredentialLookup::Unavailable(format!(
                    "no subscription signed in for managed mode (`prometheus auth login {SUBSCRIPTION}`)"
                ));
            };
            CredentialLookup::Ready(Credential {
                kind: "managed".to_owned(),
                origin: GATEWAY_ORIGIN.to_owned(),
                billing_source: Some(billing_source),
                hosts: vec![host(GATEWAY_ORIGIN)],
                headers: vec![(
                    "authorization".to_owned(),
                    SecretString::new(format!("Bearer {}", signed_in.token.expose())),
                )],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_direct_then_managed_and_names_both_when_neither() {
        assert_eq!(resolve(ModalMode::Auto, true, true), Ok(Mode::Direct));
        assert_eq!(
            resolve(ModalMode::Auto, false, true),
            Ok(Mode::Managed {
                billing_source: "subscription:nous".to_owned()
            })
        );
        let why = resolve(ModalMode::Auto, false, false).unwrap_err();
        assert!(
            why.contains("MODAL_TOKEN_ID") && why.contains("subscription"),
            "{why}"
        );
        assert!(resolve(ModalMode::Direct, false, true).is_err());
        assert!(resolve(ModalMode::Managed, true, false).is_err());
    }

    fn scope(root: &std::path::Path) -> ProfileScope {
        use pmt_config::{Home, Launch, Overrides, ProfileName};
        let home = Home::open(root).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
    }

    /// Scenario: Credential handle hides the secret (plugin-system, Host
    /// credential handles): the `modal` handle resolves to the token
    /// headers for Modal's API only, or to the reason it cannot.
    #[test]
    fn the_modal_handle_carries_the_direct_tokens_only_to_modal() {
        let dir = tempfile::tempdir().unwrap();
        let scope = scope(dir.path());
        let CredentialLookup::Unavailable(why) = credential(&scope) else {
            panic!("a credential without secrets or sign-in");
        };
        assert!(
            why.contains("MODAL_TOKEN_ID") && why.contains("subscription"),
            "{why}"
        );
        for (name, value) in [("MODAL_TOKEN_ID", "ak-1"), ("MODAL_TOKEN_SECRET", "as-1")] {
            scope
                .secrets()
                .store()
                .set(name, &SecretString::new(value))
                .unwrap();
        }
        let CredentialLookup::Ready(credential) = credential(&scope) else {
            panic!("direct mode with both secrets");
        };
        assert_eq!(credential.kind, "direct");
        assert_eq!(credential.origin, DIRECT_ORIGIN);
        assert_eq!(credential.hosts, ["api.modal.com"]);
        assert_eq!(credential.billing_source, None);
        let headers: Vec<(&str, &str)> = credential
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose()))
            .collect();
        assert_eq!(
            headers,
            [
                ("x-modal-token-id", "ak-1"),
                ("x-modal-token-secret", "as-1")
            ]
        );
        // Its debug form never shows the values.
        assert!(!format!("{credential:?}").contains("as-1"));
    }
}

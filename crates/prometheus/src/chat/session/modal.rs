//! How the first-party `modal` terminal backend authenticates (spec:
//! execution-environments, First-party backend plugins):
//! `terminal.modal.mode` `direct` uses the user's `MODAL_TOKEN_ID` and
//! `MODAL_TOKEN_SECRET` secrets, `managed` a subscription gateway
//! credential (billed as `subscription:<provider>`), and `auto` (default)
//! the first of the two that is available. `prometheus doctor terminal`
//! shows the chosen mode, or why neither is available.

use pmt_config::ProfileScope;
use pmt_config::schema::ModalMode;
use pmt_providers::resolve::CredentialExtras;

/// The secrets of `direct` mode.
pub const SECRETS: [&str; 2] = ["MODAL_TOKEN_ID", "MODAL_TOKEN_SECRET"];

/// The provider whose sign-in is the subscription gateway credential.
pub const SUBSCRIPTION: &str = "nous";

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
    let terminal = &scope.config().terminal;
    // `terminal.backend = "nous"` is the managed sandbox by definition.
    let setting = if terminal.backend == pmt_sandbox::backend::MANAGED_SANDBOX {
        ModalMode::Managed
    } else {
        terminal.modal.mode
    };
    resolve(setting, secrets, subscription)
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
}

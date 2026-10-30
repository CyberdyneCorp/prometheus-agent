//! A profile's owned credentials (spec: model-providers, Credential
//! management commands; Subscription tokens live only in the secrets
//! store).
//!
//! API keys are ordinary secrets: label `default` is the provider's first
//! secret name (`OPENAI_API_KEY`), another label `<NAME>_<LABEL>`. A
//! subscription sign-in is three secrets of the profile's store, so the
//! redactor knows each token on its own:
//! `OAUTH_<PROVIDER>_<LABEL>_ACCESS`, `..._REFRESH` and `..._META` (JSON:
//! provider, label, account id, plan, expiry, status). Nothing of a
//! sign-in is written anywhere else.

use jiff::Timestamp;
use pmt_config::secrets::SecretStore;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde::{Deserialize, Serialize};

use super::token::TokenSet;
use crate::registry::Provider;

/// The label of a provider's first credential.
pub const DEFAULT_LABEL: &str = "default";

const PREFIX: &str = "OAUTH_";
const META: &str = "_META";

/// Checks a credential label: `^[a-z0-9][a-z0-9-]{0,31}$`.
///
/// # Errors
///
/// `auth.invalid_label` (exit 2).
pub fn check_label(label: &str) -> Result<(), Error> {
    let mut chars = label.chars();
    let valid = label.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        return Ok(());
    }
    Err(Error::new(
        &codes::AUTH_INVALID_LABEL,
        format!("invalid credential label '{label}'"),
    )
    .with_hint("use 1-32 characters a-z, 0-9 or '-', e.g. personal or work-2"))
}

fn slug(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The secret holding API key `label` of `provider`; `None` when the
/// provider names no secret.
#[must_use]
pub fn api_key_secret(provider: &Provider, label: &str) -> Option<String> {
    let first = provider.secret_names().into_iter().next()?;
    Some(if label == DEFAULT_LABEL {
        first
    } else {
        format!("{first}_{}", slug(label))
    })
}

/// Whether a subscription may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Usable.
    Ok,
    /// Its refresh token was refused for good; a new sign-in is needed.
    Dead,
}

/// What the store keeps about one sign-in besides its tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    /// Provider id.
    pub provider: String,
    /// Label.
    pub label: String,
    /// How it was obtained: `browser`, `device` or `import:<tool>`.
    pub source: String,
    /// The account the tokens belong to.
    #[serde(default)]
    pub account_id: Option<String>,
    /// The plan label the provider reported.
    #[serde(default)]
    pub plan: Option<String>,
    /// Access token expiry (RFC 3339).
    #[serde(default)]
    pub expires_at: Option<String>,
    /// When it was signed in (RFC 3339).
    pub obtained_at: String,
    /// Usable or dead.
    pub status: Status,
    /// The provider refused the plan for this use (`auth.plan_not_eligible`).
    #[serde(default)]
    pub ineligible: bool,
}

impl Subscription {
    fn names(&self) -> Names {
        Names::of(&self.provider, &self.label)
    }
}

/// The three secret names of one sign-in.
struct Names {
    access: String,
    refresh: String,
    meta: String,
}

impl Names {
    fn of(provider: &str, label: &str) -> Self {
        let base = format!("{PREFIX}{}_{}", slug(provider), slug(label));
        Self {
            access: format!("{base}_ACCESS"),
            refresh: format!("{base}_REFRESH"),
            meta: format!("{base}{META}"),
        }
    }
}

/// How a new sign-in was obtained.
#[derive(Debug, Clone, Default)]
pub struct Obtained {
    /// `browser`, `device` or `import:<tool>`.
    pub source: String,
    /// The account id.
    pub account_id: Option<String>,
    /// The plan label.
    pub plan: Option<String>,
}

/// Sign-ins of one profile, over its secrets store.
#[derive(Debug, Clone)]
pub struct Credentials {
    store: SecretStore,
}

impl Credentials {
    /// The sign-ins held in `store`.
    #[must_use]
    pub fn new(store: SecretStore) -> Self {
        Self { store }
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &SecretStore {
        &self.store
    }

    /// Every sign-in, ordered by provider then label.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn subscriptions(&self) -> Result<Vec<Subscription>, Error> {
        let mut found = Vec::new();
        for name in self.store.names()? {
            if !(name.starts_with(PREFIX) && name.ends_with(META)) {
                continue;
            }
            let Some(value) = self.store.get(&name)? else {
                continue;
            };
            if let Ok(sub) = serde_json::from_str::<Subscription>(value.expose()) {
                found.push(sub);
            } else {
                tracing::warn!("secret {name} is not a readable sign-in record; ignored");
            }
        }
        found.sort_by(|a, b| (&a.provider, &a.label).cmp(&(&b.provider, &b.label)));
        Ok(found)
    }

    /// The sign-in `label` of `provider`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn subscription(&self, provider: &str, label: &str) -> Result<Option<Subscription>, Error> {
        Ok(self
            .subscriptions()?
            .into_iter()
            .find(|s| s.provider == provider && s.label == label))
    }

    /// Stores a completed sign-in, replacing one with the same label (a
    /// new sign-in clears `dead` and `ineligible`).
    ///
    /// # Errors
    ///
    /// Store errors; `auth.invalid_label` when another sign-in already uses
    /// the same secret names.
    pub fn save(
        &self,
        provider: &str,
        label: &str,
        tokens: &TokenSet,
        obtained: &Obtained,
    ) -> Result<Subscription, Error> {
        check_label(label)?;
        let names = Names::of(provider, label);
        if let Some(existing) = self.read_meta(&names.meta)?
            && (existing.provider != provider || existing.label != label)
        {
            return Err(Error::new(
                &codes::AUTH_INVALID_LABEL,
                format!(
                    "label '{label}' of '{provider}' collides with sign-in '{}' of '{}'",
                    existing.label, existing.provider
                ),
            ));
        }
        let record = Subscription {
            provider: provider.to_owned(),
            label: label.to_owned(),
            source: obtained.source.clone(),
            account_id: obtained.account_id.clone(),
            plan: obtained.plan.clone(),
            expires_at: tokens.expires_at.map(|t| t.to_string()),
            obtained_at: Timestamp::now().to_string(),
            status: Status::Ok,
            ineligible: false,
        };
        self.store.set(&names.access, &tokens.access_token)?;
        match &tokens.refresh_token {
            Some(refresh) => self.store.set(&names.refresh, refresh)?,
            None => {
                self.store.remove(&names.refresh)?;
            }
        }
        // The record goes last: it is what makes the sign-in visible.
        self.write_meta(&record)?;
        Ok(record)
    }

    /// The access token of `sub`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn access_token(&self, sub: &Subscription) -> Result<Option<SecretString>, Error> {
        self.store.get(&sub.names().access)
    }

    /// The refresh token of `sub`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn refresh_token(&self, sub: &Subscription) -> Result<Option<SecretString>, Error> {
        self.store.get(&sub.names().refresh)
    }

    /// Flags `sub` as refused for its plan (it stays `ok`).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn mark_ineligible(&self, sub: &Subscription) -> Result<(), Error> {
        let mut record = sub.clone();
        record.ineligible = true;
        self.write_meta(&record)
    }

    /// Deletes every secret of `sub`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn remove(&self, sub: &Subscription) -> Result<(), Error> {
        let names = sub.names();
        // The record goes first, so a half-removed sign-in is invisible.
        self.store.remove(&names.meta)?;
        self.store.remove(&names.access)?;
        self.store.remove(&names.refresh)?;
        Ok(())
    }

    fn read_meta(&self, name: &str) -> Result<Option<Subscription>, Error> {
        Ok(self
            .store
            .get(name)?
            .and_then(|v| serde_json::from_str(v.expose()).ok()))
    }

    fn write_meta(&self, record: &Subscription) -> Result<(), Error> {
        let json = serde_json::to_string(record).expect("records serialize");
        self.store
            .set(&record.names().meta, &SecretString::new(json))
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::SecretsBackend;
    use pmt_config::{Home, ProfileName};

    use super::*;

    fn tokens(access: &str, refresh: Option<&str>) -> TokenSet {
        TokenSet {
            access_token: SecretString::new(access.to_owned()),
            refresh_token: refresh.map(|r| SecretString::new(r.to_owned())),
            expires_at: None,
            id_token: None,
        }
    }

    #[test]
    fn a_sign_in_is_three_secrets_and_a_new_sign_in_revives_it() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let creds = Credentials::new(SecretStore::open(&profile, SecretsBackend::File));
        let obtained = Obtained {
            source: "device".to_owned(),
            ..Obtained::default()
        };
        let sub = creds
            .save(
                "openai-codex",
                "personal",
                &tokens("at-1", Some("rt-1")),
                &obtained,
            )
            .unwrap();
        let mut names = creds.store().names().unwrap();
        names.sort();
        assert_eq!(
            names,
            [
                "OAUTH_OPENAI_CODEX_PERSONAL_ACCESS",
                "OAUTH_OPENAI_CODEX_PERSONAL_META",
                "OAUTH_OPENAI_CODEX_PERSONAL_REFRESH"
            ]
        );
        creds.mark_ineligible(&sub).unwrap();
        let flagged = creds
            .subscription("openai-codex", "personal")
            .unwrap()
            .unwrap();
        assert!(flagged.ineligible);
        assert_eq!(flagged.status, Status::Ok);
        creds
            .save("openai-codex", "personal", &tokens("at-2", None), &obtained)
            .unwrap();
        let again = creds
            .subscription("openai-codex", "personal")
            .unwrap()
            .unwrap();
        assert!(!again.ineligible);
        assert_eq!(
            creds.access_token(&again).unwrap().unwrap().expose(),
            "at-2"
        );
        assert!(creds.refresh_token(&again).unwrap().is_none());
        creds.remove(&again).unwrap();
        assert!(creds.store().names().unwrap().is_empty());
    }

    #[test]
    fn labels_are_checked() {
        check_label("work-2").unwrap();
        for bad in ["", "Work", "-x", "a_b", &"x".repeat(33)] {
            assert_eq!(check_label(bad).unwrap_err().exit_code(), 2, "{bad}");
        }
    }
}

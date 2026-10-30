//! The profile's sign-ins as the resolver sees them (spec: model-providers,
//! Subscription and OAuth sign-in registry): the usable sign-in of a
//! provider, read from the secrets store at each resolution, and the
//! failure hook that flags a refused plan.

use pmt_core::error::{Error, codes};

use super::credentials::{Credentials, Status, Subscription};
use crate::resolve::{CredentialExtras, CredentialSource, Route, SignedIn};

impl Credentials {
    /// The sign-in a request to `provider` uses: a usable (`ok`) one, an
    /// eligible one before one whose plan was refused, then by label.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn usable(&self, provider: &str) -> Result<Option<Subscription>, Error> {
        let mut usable: Vec<Subscription> = self
            .subscriptions()?
            .into_iter()
            .filter(|s| s.provider == provider && s.status == Status::Ok)
            .collect();
        usable.sort_by_key(|s| s.ineligible);
        Ok(usable.into_iter().next())
    }

    /// Records what a failed call on `route` says about its credential:
    /// `auth.plan_not_eligible` flags the sign-in `ineligible` (it stays
    /// `ok`, and an API key of the provider now wins).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn note_failure(&self, route: &Route, error: &Error) -> Result<(), Error> {
        let CredentialSource::Subscription { provider, label } = &route.credential.source else {
            return Ok(());
        };
        if error.code() != &codes::AUTH_PLAN_NOT_ELIGIBLE {
            return Ok(());
        }
        match self.subscription(provider, label)? {
            Some(sub) if !sub.ineligible => self.mark_ineligible(&sub),
            _ => Ok(()),
        }
    }
}

impl CredentialExtras for Credentials {
    fn subscription(&self, provider: &str) -> Result<Option<SignedIn>, Error> {
        let Some(sub) = self.usable(provider)? else {
            return Ok(None);
        };
        let Some(token) = self.access_token(&sub)? else {
            return Ok(None);
        };
        Ok(Some(SignedIn {
            label: sub.label,
            token,
            account_id: sub.account_id,
            ineligible: sub.ineligible,
        }))
    }

    fn signed_in(&self) -> Vec<String> {
        let mut providers: Vec<String> = self
            .subscriptions()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.status == Status::Ok)
            .map(|s| s.provider)
            .collect();
        providers.dedup();
        providers
    }
}

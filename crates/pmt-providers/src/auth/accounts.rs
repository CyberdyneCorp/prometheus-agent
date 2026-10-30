//! A profile's credentials as requests use them (spec: model-providers,
//! Credential pool selection strategies; Exhausted credential cooldowns;
//! Token refresh and dead-token quarantine; Borrowed CLI logins).
//!
//! [`Accounts`] is what resolution asks for sign-ins, borrowed logins and
//! the pool state ([`CredentialExtras`]), and what the model caller runs
//! around every request ([`CredentialHooks`]): it picks the pool key,
//! re-reads (and, near expiry, refreshes) sign-in tokens, counts the
//! request, and after a failure benches the credential or refreshes a
//! refused token.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_core::error::Error;

use super::borrowed::Borrowed;
use super::credentials::{Credentials, Status, reauth_required};
use super::refresh::{EARLY, Refresher};
use super::token::RefreshError;
use crate::caller::{CredentialHooks, Recovery};
use crate::classify::{ErrorClass, ProviderError};
use crate::pool::{self, PoolState};
use crate::registry::{Provider, ProviderRegistry};
use crate::resolve::{
    BoxFuture, Credential, CredentialExtras, CredentialSource, Route, SecretSource, SignedIn,
    benched, request_key,
};

/// One profile's credentials.
pub struct Accounts {
    registry: ProviderRegistry,
    secrets: Arc<dyn SecretSource>,
    credentials: Credentials,
    state: Option<Arc<dyn PoolState>>,
    borrowed: Borrowed,
    refresher: Option<Refresher>,
}

impl std::fmt::Debug for Accounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accounts")
            .field("borrowed", &self.borrowed)
            .finish_non_exhaustive()
    }
}

impl Accounts {
    /// Keys from `secrets` and sign-ins from `credentials` of the profile
    /// whose providers are `registry`; no pool state, borrowing or refresh
    /// until added.
    #[must_use]
    pub fn new(
        registry: ProviderRegistry,
        secrets: Arc<dyn SecretSource>,
        credentials: Credentials,
    ) -> Self {
        Self {
            registry,
            secrets,
            credentials,
            state: None,
            borrowed: Borrowed::none(),
            refresher: None,
        }
    }

    /// Request counts and benches in `state` (`state.db`).
    #[must_use]
    pub fn with_state(mut self, state: Arc<dyn PoolState>) -> Self {
        self.state = Some(state);
        self
    }

    /// Other CLIs' logins that may be borrowed.
    #[must_use]
    pub fn with_borrowed(mut self, borrowed: Borrowed) -> Self {
        self.borrowed = borrowed;
        self
    }

    /// Refreshes sign-in tokens with `refresher`.
    #[must_use]
    pub fn with_refresher(mut self, refresher: Refresher) -> Self {
        self.refresher = Some(refresher);
        self
    }

    /// The owned sign-ins.
    #[must_use]
    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    /// The borrowable logins.
    #[must_use]
    pub fn borrowed_logins(&self) -> &Borrowed {
        &self.borrowed
    }

    /// The pool state.
    #[must_use]
    pub fn state(&self) -> Option<&dyn PoolState> {
        self.state.as_deref()
    }

    /// Whether `provider` has at most one credential that is not dead.
    fn sole(&self, provider: &Provider) -> bool {
        let keys = self
            .secrets
            .names()
            .map_or(0, |names| pool::key_members(provider, &names).len());
        let sign_ins = self.credentials.subscriptions().map_or(0, |subs| {
            subs.iter()
                .filter(|s| s.provider == provider.id() && s.status == Status::Ok)
                .count()
        });
        keys + sign_ins <= 1
    }

    fn benched_until(
        &self,
        provider: &str,
        id: &str,
        now: Timestamp,
    ) -> Result<Option<Timestamp>, Error> {
        let Some(state) = &self.state else {
            return Ok(None);
        };
        Ok(pool::benched_until(&state.states(provider)?, id, now))
    }

    fn bench(&self, provider: &Provider, id: &str, error: &ProviderError, now: Timestamp) {
        let Some(state) = &self.state else {
            return;
        };
        let Some((until, reason)) = pool::cooldown(error, self.sole(provider), now) else {
            return;
        };
        tracing::info!(provider = provider.id(), credential = id, %until, reason, "credential benched");
        if let Err(err) = state.bench(provider.id(), id, Some(until), Some(reason)) {
            tracing::warn!(error = %err, "cannot record a credential bench");
        }
    }

    /// The credential for one request on `route`.
    async fn credential_for(
        &self,
        provider: &Provider,
        route: &Route,
    ) -> Result<Credential, Error> {
        let now = Timestamp::now();
        if let Some(id) = route.credential.state_id()
            && let Some(until) = self.benched_until(provider.id(), &id, now)?
            && !matches!(route.credential.source, CredentialSource::Pool { .. })
        {
            return Err(benched(provider.id(), until));
        }
        match &route.credential.source {
            CredentialSource::Pool { .. } => {
                match request_key(
                    &self.registry,
                    &*self.secrets,
                    self.state(),
                    provider,
                    &route.base_url,
                )? {
                    Ok(Some(key)) => Ok(key),
                    Ok(None) => Ok(route.credential.clone()),
                    Err(until) => Err(benched(provider.id(), until)),
                }
            }
            CredentialSource::Subscription { label, .. } => {
                let signed_in = self.owned_token(provider, label, now).await?;
                Ok(Credential::subscription(provider.id(), signed_in))
            }
            CredentialSource::Borrowed { cli, .. } => {
                let login = self
                    .borrowed
                    .of_cli(cli)
                    .and_then(|l| l.signed_in().transpose());
                match login {
                    Some(signed_in) => Ok(Credential::subscription(provider.id(), signed_in?)),
                    None => Err(reauth_required(provider.id(), &format!("borrowed:{cli}"))),
                }
            }
            CredentialSource::Flag | CredentialSource::Secret { .. } | CredentialSource::None => {
                Ok(route.credential.clone())
            }
        }
    }

    /// The current token of owned sign-in `label`, refreshed under the
    /// lock when it expires within a minute.
    async fn owned_token(
        &self,
        provider: &Provider,
        label: &str,
        now: Timestamp,
    ) -> Result<SignedIn, Error> {
        let sub = self
            .credentials
            .subscription(provider.id(), label)?
            .filter(|s| s.status == Status::Ok)
            .ok_or_else(|| reauth_required(provider.id(), label))?;
        let expiring = sub
            .expires_at
            .as_deref()
            .and_then(|at| at.parse::<Timestamp>().ok())
            .is_some_and(|at| at <= pool::later(now, EARLY));
        if let (true, Some(refresher)) = (expiring, &self.refresher) {
            return refresher
                .owned(&self.credentials, provider, label, None)
                .await
                .map_err(|e| {
                    self.refresh_error(provider, &pool::subscription_id(label), label, e)
                });
        }
        let token = self
            .credentials
            .access_token(&sub)?
            .ok_or_else(|| reauth_required(provider.id(), label))?;
        Ok(SignedIn {
            label: sub.label,
            token,
            account_id: sub.account_id,
            ineligible: sub.ineligible,
            borrowed: None,
        })
    }

    /// The error of a failed refresh; a rate-limited token endpoint
    /// benches the credential.
    fn refresh_error(
        &self,
        provider: &Provider,
        id: &str,
        label: &str,
        error: RefreshError,
    ) -> Error {
        match error {
            RefreshError::Dead(_) => reauth_required(provider.id(), label),
            RefreshError::RateLimited => {
                let now = Timestamp::now();
                let throttled = ProviderError {
                    status: Some(429),
                    ..ProviderError::new(ErrorClass::RateLimit, "token endpoint rate limit")
                };
                self.bench(provider, id, &throttled, now);
                let until = self
                    .benched_until(provider.id(), id, now)
                    .ok()
                    .flatten()
                    .unwrap_or(now);
                benched(provider.id(), until)
            }
            RefreshError::Failed(err) => err,
        }
    }

    /// After HTTP 401 on a sign-in: refreshes the refused token.
    async fn recover(&self, provider: &Provider, route: &Route) -> Recovery {
        let (Some(refresher), Some(refused)) = (&self.refresher, route.credential.value()) else {
            return Recovery::Continue;
        };
        let (outcome, id, label) = match &route.credential.source {
            CredentialSource::Subscription { label, .. } => (
                refresher
                    .owned(&self.credentials, provider, label, Some(refused))
                    .await,
                pool::subscription_id(label),
                label.clone(),
            ),
            CredentialSource::Borrowed { cli, .. } => {
                let Some(login) = self.borrowed.of_cli(cli) else {
                    return Recovery::Continue;
                };
                (
                    refresher.borrowed(login, provider, refused).await,
                    pool::borrowed_id(cli),
                    login.label(),
                )
            }
            _ => return Recovery::Continue,
        };
        match outcome {
            Ok(_) => Recovery::RetryNow,
            Err(error @ (RefreshError::Dead(_) | RefreshError::RateLimited)) => {
                let error = self.refresh_error(provider, &id, &label, error);
                Recovery::Fail(ProviderError::refused(ErrorClass::Auth, &error))
            }
            Err(RefreshError::Failed(err)) => {
                tracing::warn!(provider = provider.id(), error = %err, "token refresh failed");
                Recovery::Continue
            }
        }
    }

    fn record_use(&self, provider: &str, credential: &Credential) {
        let (Some(state), Some(id)) = (&self.state, credential.state_id()) else {
            return;
        };
        if let Err(err) = state.record_use(provider, &id, Timestamp::now()) {
            tracing::warn!(error = %err, "cannot count a credential request");
        }
    }
}

/// The refusal class of a credential error: benches are rate limits,
/// everything else is an authentication problem.
fn refusal(error: &Error) -> ProviderError {
    let class = if error.code() == &pmt_core::error::codes::AUTH_CREDENTIALS_BENCHED {
        ErrorClass::RateLimit
    } else {
        ErrorClass::Auth
    };
    ProviderError::refused(class, error)
}

impl CredentialExtras for Accounts {
    fn subscription(&self, provider: &str) -> Result<Option<SignedIn>, Error> {
        CredentialExtras::subscription(&self.credentials, provider)
    }

    fn borrowed(&self, provider: &str) -> Result<Option<SignedIn>, Error> {
        match self.borrowed.login(provider) {
            Some(login) => login.signed_in(),
            None => Ok(None),
        }
    }

    fn dead_sign_in(&self, provider: &str) -> Result<Option<String>, Error> {
        self.credentials.dead_sign_in(provider)
    }

    fn signed_in(&self) -> Vec<String> {
        CredentialExtras::signed_in(&self.credentials)
    }

    fn pool_state(&self) -> Option<&dyn PoolState> {
        self.state()
    }
}

impl CredentialHooks for Accounts {
    fn prepare<'a>(&'a self, route: &'a Route) -> BoxFuture<'a, Result<Route, ProviderError>> {
        Box::pin(async move {
            let Ok(provider) = self.registry.lookup(&route.provider) else {
                return Ok(route.clone());
            };
            let credential = self
                .credential_for(provider, route)
                .await
                .map_err(|e| refusal(&e))?;
            self.record_use(provider.id(), &credential);
            Ok(Route {
                credential,
                ..route.clone()
            })
        })
    }

    fn failed<'a>(
        &'a self,
        route: &'a Route,
        error: &'a ProviderError,
        may_recover: bool,
    ) -> BoxFuture<'a, Recovery> {
        Box::pin(async move {
            if error.refusal.is_some() {
                return Recovery::Continue;
            }
            let Ok(provider) = self.registry.lookup(&route.provider) else {
                return Recovery::Continue;
            };
            if may_recover && error.status == Some(401) && route.credential.is_subscription() {
                return self.recover(provider, route).await;
            }
            if let Some(id) = route.credential.state_id() {
                self.bench(provider, &id, error, Timestamp::now());
            }
            Recovery::Continue
        })
    }
}

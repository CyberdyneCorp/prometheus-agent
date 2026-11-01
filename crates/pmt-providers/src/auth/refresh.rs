//! Token refresh (spec: model-providers, Subscription tokens live only in
//! the secrets store; Token refresh and dead-token quarantine; Borrowed CLI
//! logins).
//!
//! Every refresh of one credential runs under an exclusive file lock shared
//! by all processes of the profile (`<profile>/cache/locks/`), and re-reads
//! the stored token after taking it: when another process already
//! refreshed, its rotated token is used and the single-use refresh token
//! is never presented twice. A terminal refusal quarantines an owned
//! sign-in (`dead`, refresh token deleted); a borrowed login is never
//! altered beyond writing rotated tokens back.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::secret::SecretString;

use super::borrowed::ExternalLogin;
use super::credentials::{Credentials, Status, Subscription};
use super::spec;
use super::token::{self, RefreshError, TokenSet};
use crate::pool;
use crate::registry::Provider;
use crate::resolve::SignedIn;

/// An access token is refreshed when it expires within this.
pub const EARLY: Duration = Duration::from_mins(1);

/// Refreshes sign-ins of one profile.
#[derive(Debug, Clone)]
pub struct Refresher {
    client: reqwest::Client,
    lock_dir: PathBuf,
}

impl Refresher {
    /// Refreshes with `client`, locking in `lock_dir`.
    #[must_use]
    pub fn new(client: reqwest::Client, lock_dir: PathBuf) -> Self {
        Self { client, lock_dir }
    }

    /// The lock directory of the profile at `profile_dir`.
    #[must_use]
    pub fn lock_dir_of(profile_dir: &Path) -> PathBuf {
        profile_dir.join("cache").join("locks")
    }

    /// Waits for the exclusive lock `name` (released when the file drops).
    async fn lock(&self, name: String) -> Result<File, RefreshError> {
        let dir = self.lock_dir.clone();
        let locked = tokio::task::spawn_blocking(move || {
            private_fs::create_dir(&dir)?;
            private_fs::lock_beside(&dir.join(name))
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
        .and_then(|r| r);
        locked.map_err(|e| {
            RefreshError::Failed(Error::new(
                &codes::AUTH_REFRESH_FAILED,
                format!("cannot take the token refresh lock: {e}"),
            ))
        })
    }

    /// The token of owned sign-in `label` of `provider`: refreshed when it
    /// expires within [`EARLY`], or, with `refused` (the token an HTTP 401
    /// just refused), when the store still holds that token.
    ///
    /// # Errors
    ///
    /// [`RefreshError::Dead`] for a quarantined sign-in (now or before),
    /// [`RefreshError::RateLimited`], [`RefreshError::Failed`].
    pub async fn owned(
        &self,
        credentials: &Credentials,
        provider: &Provider,
        label: &str,
        refused: Option<&SecretString>,
    ) -> Result<SignedIn, RefreshError> {
        let lock_name = format!("refresh-{}-{label}", provider.id());
        let _lock = self.lock(lock_name).await?;
        let failed = RefreshError::Failed;
        let sub = credentials
            .subscription(provider.id(), label)
            .map_err(failed)?
            .ok_or_else(|| RefreshError::Dead("the sign-in was removed".to_owned()))?;
        if sub.status == Status::Dead {
            return Err(RefreshError::Dead("the sign-in is quarantined".to_owned()));
        }
        let access = credentials
            .access_token(&sub)
            .map_err(failed)?
            .ok_or_else(|| RefreshError::Dead("the sign-in has no access token".to_owned()))?;
        let stale = match refused {
            Some(refused) => refused.expose() == access.expose(),
            None => expiring(&sub, Timestamp::now()),
        };
        if !stale {
            return Ok(signed_in(sub, access));
        }
        let Some(refresh) = credentials.refresh_token(&sub).map_err(failed)? else {
            credentials.mark_dead(&sub).map_err(failed)?;
            return Err(RefreshError::Dead(
                "the sign-in has no refresh token".to_owned(),
            ));
        };
        let spec = spec::for_provider(provider).map_err(failed)?;
        spec.check_token_endpoint().map_err(failed)?;
        match token::refresh(&self.client, &spec, &refresh).await {
            Ok(tokens) => {
                let record = credentials.update_tokens(&sub, &tokens).map_err(failed)?;
                Ok(signed_in(record, tokens.access_token))
            }
            Err(RefreshError::Dead(why)) => {
                tracing::warn!(provider = provider.id(), label, %why, "sign-in quarantined: its refresh token was refused");
                credentials.mark_dead(&sub).map_err(failed)?;
                Err(RefreshError::Dead(why))
            }
            Err(other) => Err(other),
        }
    }

    /// Refreshes the borrowed `login` after its token `refused` was refused
    /// (HTTP 401) and writes the rotated tokens back to the CLI's store;
    /// when the store already holds another token (the CLI or another
    /// process refreshed), that one is used without a refresh.
    ///
    /// # Errors
    ///
    /// [`RefreshError::Dead`] when the login is gone or its refresh token
    /// was refused, [`RefreshError::RateLimited`], [`RefreshError::Failed`]
    /// (including a failed write-back).
    pub async fn borrowed(
        &self,
        login: &ExternalLogin,
        provider: &Provider,
        refused: &SecretString,
    ) -> Result<SignedIn, RefreshError> {
        let _lock = self.lock(format!("borrowed-{}", login.cli())).await?;
        let current = login
            .read()
            .map_err(RefreshError::Failed)?
            .ok_or_else(|| RefreshError::Dead("the borrowed login is gone".to_owned()))?;
        if current.access.expose() != refused.expose() {
            return Ok(login.to_signed_in(current));
        }
        let refresh = current.refresh.clone().ok_or_else(|| {
            RefreshError::Dead("the borrowed login has no refresh token".to_owned())
        })?;
        let spec = spec::for_provider(provider).map_err(RefreshError::Failed)?;
        spec.check_token_endpoint().map_err(RefreshError::Failed)?;
        let mut tokens = token::refresh(&self.client, &spec, &refresh).await?;
        if tokens.refresh_token.is_none() {
            tokens.refresh_token = Some(refresh);
        }
        login.write_back(&tokens).map_err(|e| {
            RefreshError::Failed(Error::new(
                &codes::AUTH_REFRESH_FAILED,
                format!(
                    "the refreshed {} login could not be written back: {}",
                    login.cli(),
                    e.message()
                ),
            ))
        })?;
        Ok(login.to_signed_in(borrowed_tokens(&tokens, current)))
    }
}

fn expiring(sub: &Subscription, now: Timestamp) -> bool {
    sub.expires_at
        .as_deref()
        .and_then(|at| at.parse::<Timestamp>().ok())
        .is_some_and(|at| at <= pool::later(now, EARLY))
}

fn signed_in(sub: Subscription, token: SecretString) -> SignedIn {
    SignedIn {
        label: sub.label,
        token,
        account_id: sub.account_id,
        ineligible: sub.ineligible,
        borrowed: None,
    }
}

fn borrowed_tokens(
    tokens: &TokenSet,
    before: super::borrowed::BorrowedTokens,
) -> super::borrowed::BorrowedTokens {
    super::borrowed::BorrowedTokens {
        access: tokens.access_token.clone(),
        refresh: tokens.refresh_token.clone(),
        expires_at: tokens.expires_at,
        ..before
    }
}

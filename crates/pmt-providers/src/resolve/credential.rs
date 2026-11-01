//! Credential resolution (spec: model-providers, Deterministic explainable
//! resolution; Credentials are bound to their authoritative hosts).
//!
//! Order: a key given for this launch, a pooled key, a sign-in token
//! (owned, else borrowed from another agent CLI), then the stored secret
//! bound to the request's host; benched credentials are skipped. A
//! secret is bound to the `key_hosts` of every provider whose descriptor
//! names it (plus `providers.<id>.key_hosts` of the requesting provider);
//! a secret no descriptor names is bound to the requesting provider's own
//! hosts. Matching is exact or an explicit `*.` wildcard, never a suffix.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use jiff::Timestamp;
use pmt_config::SecretsHandle;
use pmt_config::provider_urls::{host_of, is_local_host};
use pmt_config::schema::{PreferCredential, ProviderAuth};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use pmt_store::credential_state::CredentialState;

use super::Resolver;
use crate::descriptor::host_matches;
use crate::pool::{self, PoolState};
use crate::registry::{Provider, ProviderRegistry};

/// Reads secrets by name.
pub trait SecretSource: Send + Sync {
    /// The value of `name`, `None` when it is not set.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error>;

    /// Every set secret name (pools look for labelled keys among them).
    ///
    /// # Errors
    ///
    /// Store errors.
    fn names(&self) -> Result<Vec<String>, Error>;
}

impl SecretSource for SecretsHandle {
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error> {
        match self.resolve(name) {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.code() == &codes::SECRET_MISSING => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn names(&self) -> Result<Vec<String>, Error> {
        let mut names: Vec<String> = self.list()?.into_iter().map(|row| row.name).collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

impl SecretSource for BTreeMap<String, SecretString> {
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error> {
        Ok(self.get(name).cloned())
    }

    fn names(&self) -> Result<Vec<String>, Error> {
        Ok(self.keys().cloned().collect())
    }
}

/// A subscription sign-in a request may use.
#[derive(Debug, Clone)]
pub struct SignedIn {
    /// The sign-in's label.
    pub label: String,
    /// The access token.
    pub token: SecretString,
    /// The account the token belongs to, when the provider routes by it
    /// (ChatGPT plans).
    pub account_id: Option<String>,
    /// The provider refused the plan for this use (`auth.plan_not_eligible`);
    /// an API key then wins whatever `prefer_credential` says.
    pub ineligible: bool,
    /// The agent CLI whose login this is, when it is borrowed rather than
    /// owned by the profile.
    pub borrowed: Option<String>,
}

/// Credentials beyond plain secrets: sign-ins, borrowed logins and the
/// pool state.
pub trait CredentialExtras: Send + Sync {
    /// The usable sign-in the profile owns for `provider`, if any.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn subscription(&self, _provider: &str) -> Result<Option<SignedIn>, Error> {
        Ok(None)
    }

    /// A login borrowed from another agent CLI for `provider`; asked only
    /// when the profile owns no usable sign-in for it.
    ///
    /// # Errors
    ///
    /// Read errors of the other CLI's store.
    fn borrowed(&self, _provider: &str) -> Result<Option<SignedIn>, Error> {
        Ok(None)
    }

    /// The label of a quarantined (`dead`) sign-in of `provider`.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn dead_sign_in(&self, _provider: &str) -> Result<Option<String>, Error> {
        Ok(None)
    }

    /// Providers with a sign-in, in sign-in order.
    fn signed_in(&self) -> Vec<String> {
        Vec::new()
    }

    /// Request counts and benches; `None`: nothing benched, pools rotate
    /// from their first key.
    fn pool_state(&self) -> Option<&dyn PoolState> {
        None
    }
}

/// No pools and no sign-ins.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoExtras;

impl CredentialExtras for NoExtras {}

/// Where a route's credential came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// A key given for this launch only.
    Flag,
    /// A key of the provider's credential pool.
    Pool {
        /// Provider id.
        provider: String,
        /// 1-based pool position.
        index: usize,
        /// Label, if any.
        label: Option<String>,
        /// The secret holding it.
        secret: String,
    },
    /// A subscription sign-in the profile owns.
    Subscription {
        /// Provider id.
        provider: String,
        /// The sign-in's label.
        label: String,
    },
    /// A login borrowed from another agent CLI, read in place.
    Borrowed {
        /// Provider id.
        provider: String,
        /// The CLI (`claude-code`, `codex-cli`).
        cli: String,
    },
    /// A stored secret.
    Secret {
        /// Its name.
        name: String,
    },
    /// No credential (`auth = "none"`).
    None,
}

/// A route's credential; the value never appears in `Debug` or traces.
/// Two credentials are equal when they come from the same source.
#[derive(Debug, Clone)]
pub struct Credential {
    /// Where it came from.
    pub source: CredentialSource,
    value: Option<SecretString>,
    account_id: Option<String>,
}

impl PartialEq for Credential {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl Credential {
    /// A credential from `source` with `value`.
    #[must_use]
    pub fn new(source: CredentialSource, value: Option<SecretString>) -> Self {
        Self {
            source,
            value,
            account_id: None,
        }
    }

    /// The credential of sign-in `signed_in` of `provider` (owned or
    /// borrowed).
    #[must_use]
    pub fn subscription(provider: &str, signed_in: SignedIn) -> Self {
        let provider = provider.to_owned();
        let source = match signed_in.borrowed {
            Some(cli) => CredentialSource::Borrowed { provider, cli },
            None => CredentialSource::Subscription {
                provider,
                label: signed_in.label,
            },
        };
        Self {
            source,
            value: Some(signed_in.token),
            account_id: signed_in.account_id,
        }
    }

    /// The account a subscription token belongs to (sent as
    /// `ChatGPT-Account-ID` on ChatGPT plans).
    #[must_use]
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    /// A subscription sign-in, owned or borrowed (sent as a bearer token,
    /// with the provider's subscription request identity).
    #[must_use]
    pub fn is_subscription(&self) -> bool {
        matches!(
            self.source,
            CredentialSource::Subscription { .. } | CredentialSource::Borrowed { .. }
        )
    }

    /// No credential.
    #[must_use]
    pub fn none() -> Self {
        Self::new(CredentialSource::None, None)
    }

    /// The value to send.
    #[must_use]
    pub fn value(&self) -> Option<&SecretString> {
        self.value.as_ref()
    }

    /// Sent as a bearer token even on Anthropic Messages (sign-ins).
    #[must_use]
    pub fn is_bearer(&self) -> bool {
        self.is_subscription()
    }

    /// The id request counts and benches are kept under
    /// ([`crate::pool`]); `None` for launch keys and keyless requests.
    #[must_use]
    pub fn state_id(&self) -> Option<String> {
        match &self.source {
            CredentialSource::Pool { secret, .. } => Some(pool::secret_id(secret)),
            CredentialSource::Secret { name } => Some(pool::secret_id(name)),
            CredentialSource::Subscription { label, .. } => Some(pool::subscription_id(label)),
            CredentialSource::Borrowed { cli, .. } => Some(pool::borrowed_id(cli)),
            CredentialSource::Flag | CredentialSource::None => None,
        }
    }

    /// `flag`, `pool:<provider>#<n>`, `subscription:<provider>/<label>`
    /// (`<label>` is `borrowed:<cli>` for a borrowed login),
    /// `secret:<NAME>` or `none`.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.source {
            CredentialSource::Flag => "flag".to_owned(),
            CredentialSource::Pool {
                provider, index, ..
            } => format!("pool:{provider}#{index}"),
            CredentialSource::Subscription { provider, label } => {
                format!("subscription:{provider}/{label}")
            }
            CredentialSource::Borrowed { provider, cli } => {
                format!("subscription:{provider}/borrowed:{cli}")
            }
            CredentialSource::Secret { name } => format!("secret:{name}"),
            CredentialSource::None => "none".to_owned(),
        }
    }

    /// The ledger's `billing_source` (design D16, Billing labels):
    /// sign-ins are `subscription:<provider>/<label>`, keys
    /// `api_key:<provider>[/<label>]`, keyless calls `local` on a local
    /// server and `keyless:<provider>` elsewhere.
    #[must_use]
    pub fn billing_source(&self, provider: &str, local: bool) -> String {
        match &self.source {
            CredentialSource::Subscription { label, .. } => {
                format!("subscription:{provider}/{label}")
            }
            CredentialSource::Borrowed { cli, .. } => {
                format!("subscription:{provider}/borrowed:{cli}")
            }
            CredentialSource::Pool {
                label: Some(label), ..
            } => format!("api_key:{provider}/{label}"),
            CredentialSource::Flag
            | CredentialSource::Pool { .. }
            | CredentialSource::Secret { .. } => {
                format!("api_key:{provider}")
            }
            CredentialSource::None if local => "local".to_owned(),
            CredentialSource::None => format!("keyless:{provider}"),
        }
    }

    /// This credential with a new value (a refreshed or re-read token).
    #[must_use]
    pub fn with_value(mut self, value: SecretString) -> Self {
        self.value = Some(value);
        self
    }
}

/// What resolution found besides the credential it returns.
#[derive(Default)]
struct Seen {
    /// Credentials that exist but may not be sent to the host.
    unbound: Vec<String>,
    /// The earliest end of a bench that kept a credential out.
    benched: Option<Timestamp>,
}

impl Seen {
    fn bench(&mut self, until: Timestamp) {
        self.benched = Some(self.benched.map_or(until, |b| b.min(until)));
    }
}

/// Resolves the credential of `provider` for a request to `base_url`;
/// `explicit` names a secret replacing the provider's own names.
///
/// Order: a launch key; the provider's key pool (two or more keys bound
/// to the host); between a sign-in (owned, else borrowed) and a single
/// key, `providers.<id>.prefer_credential` decides (subscription by
/// default) unless the sign-in's plan was found ineligible; keyless.
/// Benched credentials are skipped; when nothing else is left the error
/// is `auth.credentials_benched`, or `auth.reauth_required` for a dead
/// sign-in.
pub(super) fn resolve(
    resolver: &Resolver<'_>,
    provider: &Provider,
    base_url: &str,
    explicit: Option<&str>,
) -> Result<Credential, Error> {
    let id = provider.id();
    if let Some(key) = resolver.ad_hoc.as_ref().and_then(|a| a.key_for(base_url)) {
        return Ok(Credential::new(CredentialSource::Flag, Some(key.clone())));
    }
    let host = host_of(base_url).unwrap_or_default();
    let now = Timestamp::now();
    let states = match resolver.extras.pool_state() {
        Some(state) => state.states(id)?,
        None => Vec::new(),
    };
    let mut seen = Seen::default();
    let names: Vec<String> =
        explicit.map_or_else(|| provider.secret_names(), |n| vec![n.to_owned()]);
    let keys = bound_keys(
        resolver.registry,
        resolver.secrets,
        provider,
        &host,
        explicit,
        &mut seen,
    )?;
    let key = pick_key(provider, keys, &states, now, &mut seen);
    if let Some(pooled) = key
        .as_ref()
        .filter(|k| matches!(k.source, CredentialSource::Pool { .. }))
    {
        return Ok(pooled.clone());
    }
    let mut subscription = None;
    if explicit.is_none() {
        let prefer_subscription =
            provider.settings().prefer_credential == PreferCredential::Subscription;
        let want_borrowed = prefer_subscription || key.is_none();
        subscription = bound_subscription(resolver, provider, &host, want_borrowed, &mut seen)?
            .filter(|s| usable_sign_in(s, &states, now, &mut seen));
        if let Some(signed_in) = subscription.take_if(|s| prefer_subscription && !s.ineligible) {
            return Ok(Credential::subscription(id, signed_in));
        }
    }
    if let Some(key) = key {
        return Ok(key);
    }
    if let Some(signed_in) = subscription {
        return Ok(Credential::subscription(id, signed_in));
    }
    let loopback_keyless = provider.descriptor().local || provider.descriptor().keyless_on_loopback;
    let keyless = provider.auth() == ProviderAuth::None
        || (loopback_keyless && explicit.is_none() && is_local_host(&host));
    if keyless {
        return Ok(Credential::none());
    }
    Err(unusable(resolver, id, &host, &names, &seen)?)
}

/// The provider's keys that are set and bound to `host` (or only the
/// `explicit` secret), in pool order.
fn bound_keys(
    registry: &ProviderRegistry,
    secrets: &dyn SecretSource,
    provider: &Provider,
    host: &str,
    explicit: Option<&str>,
    seen: &mut Seen,
) -> Result<Vec<(pool::Member, SecretString)>, Error> {
    let members = match explicit {
        Some(name) => vec![pool::Member {
            label: name.to_owned(),
            secret: name.to_owned(),
            pooled: true,
        }],
        None => pool::key_members(provider, &secrets.names()?),
    };
    let mut keys = Vec::new();
    for member in members {
        let Some(value) = secrets.secret(&member.secret)? else {
            continue;
        };
        let hosts = bound_hosts(registry, provider, &member.secret);
        if hosts.iter().any(|pattern| host_matches(pattern, host)) {
            keys.push((member, value));
        } else {
            seen.unbound.push(format!(
                "{} is bound to {}",
                member.secret,
                hosts.join(", ")
            ));
        }
    }
    Ok(keys)
}

/// The provider's key for one request to `base_url`: by `pool_strategy`
/// among its pool, else its single key; `Err(until)` when every key is
/// benched, `Ok(None)` when it has no key bound to the host.
///
/// # Errors
///
/// Store errors (outer).
pub(crate) fn request_key(
    registry: &ProviderRegistry,
    secrets: &dyn SecretSource,
    state: Option<&dyn PoolState>,
    provider: &Provider,
    base_url: &str,
) -> Result<Result<Option<Credential>, Timestamp>, Error> {
    let host = host_of(base_url).unwrap_or_default();
    let now = Timestamp::now();
    let states = match state {
        Some(state) => state.states(provider.id())?,
        None => Vec::new(),
    };
    let mut seen = Seen::default();
    let keys = bound_keys(registry, secrets, provider, &host, None, &mut seen)?;
    Ok(
        match (
            pick_key(provider, keys, &states, now, &mut seen),
            seen.benched,
        ) {
            (Some(key), _) => Ok(Some(key)),
            (None, Some(until)) => Err(until),
            (None, None) => Ok(None),
        },
    )
}

/// One key: by `pool_strategy` among the pool keys (a `pool:` credential
/// when there are two or more), else the first alternative secret name
/// that is not benched.
fn pick_key(
    provider: &Provider,
    keys: Vec<(pool::Member, SecretString)>,
    states: &[CredentialState],
    now: Timestamp,
    seen: &mut Seen,
) -> Option<Credential> {
    let ids: Vec<String> = keys
        .iter()
        .map(|(m, _)| pool::secret_id(&m.secret))
        .collect();
    for id in &ids {
        if let Some(until) = pool::benched_until(states, id, now) {
            seen.bench(until);
        }
    }
    let (pooled, alternates): (Vec<_>, Vec<_>) =
        keys.into_iter().zip(ids).partition(|((m, _), _)| m.pooled);
    let pool_ids: Vec<String> = pooled.iter().map(|(_, id)| id.clone()).collect();
    let several = pooled.len() > 1;
    if let Some(index) = pool::select(provider.settings().pool_strategy, &pool_ids, states, now) {
        let ((member, value), _) = pooled.into_iter().nth(index)?;
        let source = if several {
            CredentialSource::Pool {
                provider: provider.id().to_owned(),
                index: index + 1,
                label: Some(member.label),
                secret: member.secret,
            }
        } else {
            CredentialSource::Secret {
                name: member.secret,
            }
        };
        return Some(Credential::new(source, Some(value)));
    }
    let ((member, value), _) = alternates
        .into_iter()
        .find(|(_, id)| pool::benched_until(states, id, now).is_none())?;
    let source = CredentialSource::Secret {
        name: member.secret,
    };
    Some(Credential::new(source, Some(value)))
}

/// Whether sign-in `s` is not benched (recording the bench when it is).
fn usable_sign_in(
    s: &SignedIn,
    states: &[CredentialState],
    now: Timestamp,
    seen: &mut Seen,
) -> bool {
    let id = s
        .borrowed
        .as_deref()
        .map_or_else(|| pool::subscription_id(&s.label), pool::borrowed_id);
    match pool::benched_until(states, &id, now) {
        Some(until) => {
            seen.bench(until);
            false
        }
        None => true,
    }
}

/// The provider's sign-in (owned; else, when `want_borrowed`, one
/// borrowed from another CLI) when `host` is one of the provider's own
/// key hosts (a sign-in token is bound like a key).
fn bound_subscription(
    resolver: &Resolver<'_>,
    provider: &Provider,
    host: &str,
    want_borrowed: bool,
    seen: &mut Seen,
) -> Result<Option<SignedIn>, Error> {
    let signed_in = match resolver.extras.subscription(provider.id())? {
        Some(owned) => owned,
        None if want_borrowed => match resolver.extras.borrowed(provider.id())? {
            Some(borrowed) => borrowed,
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    let hosts = provider.key_hosts();
    if hosts.iter().any(|pattern| host_matches(pattern, host)) {
        return Ok(Some(signed_in));
    }
    seen.unbound.push(format!(
        "sign-in '{}' is bound to {}",
        signed_in.label,
        hosts.join(", ")
    ));
    Ok(None)
}

/// The hosts secret `name` may be sent to on behalf of `provider`.
pub(crate) fn bound_hosts(
    registry: &ProviderRegistry,
    provider: &Provider,
    name: &str,
) -> Vec<String> {
    let mut hosts: Vec<String> = registry
        .claiming_secret(name)
        .flat_map(|p| p.descriptor().key_hosts.clone())
        .collect();
    if hosts.is_empty() {
        hosts.clone_from(&provider.descriptor().key_hosts);
    }
    hosts.extend(provider.settings().key_hosts.iter().cloned());
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Why no credential is usable: benched, a dead sign-in, or missing.
fn unusable(
    resolver: &Resolver<'_>,
    provider: &str,
    host: &str,
    names: &[String],
    seen: &Seen,
) -> Result<Error, Error> {
    if let Some(until) = seen.benched {
        return Ok(benched(provider, until));
    }
    if let Some(label) = resolver.extras.dead_sign_in(provider)? {
        return Ok(crate::auth::reauth_required(provider, &label));
    }
    Ok(missing(provider, host, names, &seen.unbound))
}

/// `auth.credentials_benched` for `provider` until `until`.
#[must_use]
pub fn benched(provider: &str, until: Timestamp) -> Error {
    Error::new(
        &codes::AUTH_CREDENTIALS_BENCHED,
        format!(
            "every credential of '{provider}' is benched until {}",
            until.strftime("%Y-%m-%dT%H:%M:%SZ")
        ),
    )
    .with_hint(format!("prometheus auth status {provider}"))
}

fn missing(provider: &str, host: &str, names: &[String], unbound: &[String]) -> Error {
    let mut message = format!("no credential for provider '{provider}' may be sent to {host}");
    if names.is_empty() {
        message.push_str(" (it has no secret names; set providers.<id>.key_secret)");
    } else {
        let _ = write!(message, " (checked: {})", names.join(", "));
    }
    if !unbound.is_empty() {
        let _ = write!(message, "; {}", unbound.join("; "));
    }
    let hint = names.first().map_or_else(
        || format!("prometheus config set providers.{provider}.key_secret <NAME>"),
        |name| format!("prometheus auth add {provider} (or: prometheus secrets set {name})"),
    );
    Error::new(&codes::PROVIDER_MISSING_CREDENTIAL, message).with_hint(hint)
}

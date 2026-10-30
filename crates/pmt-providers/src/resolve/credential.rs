//! Credential resolution (spec: model-providers, Deterministic explainable
//! resolution; Credentials are bound to their authoritative hosts).
//!
//! Order: a key given for this launch, a pooled credential, a sign-in
//! token, then the first stored secret bound to the request's host. A
//! secret is bound to the `key_hosts` of every provider whose descriptor
//! names it (plus `providers.<id>.key_hosts` of the requesting provider);
//! a secret no descriptor names is bound to the requesting provider's own
//! hosts. Matching is exact or an explicit `*.` wildcard, never a suffix.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pmt_config::SecretsHandle;
use pmt_config::provider_urls::{host_of, is_local_host};
use pmt_config::schema::{PreferCredential, ProviderAuth};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use super::Resolver;
use crate::descriptor::host_matches;
use crate::registry::Provider;

/// Reads secrets by name.
pub trait SecretSource: Send + Sync {
    /// The value of `name`, `None` when it is not set.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error>;
}

impl SecretSource for SecretsHandle {
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error> {
        match self.resolve(name) {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.code() == &codes::SECRET_MISSING => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl SecretSource for BTreeMap<String, SecretString> {
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error> {
        Ok(self.get(name).cloned())
    }
}

/// A pooled credential (pools are owned by the credential-pool task; the
/// resolver only asks for the next one).
#[derive(Debug, Clone)]
pub struct Pooled {
    /// 1-based position in the provider's pool.
    pub index: usize,
    /// The credential's label, if any.
    pub label: Option<String>,
    /// The value.
    pub value: SecretString,
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
}

/// Credentials beyond plain secrets: pools and sign-in tokens.
pub trait CredentialExtras: Send + Sync {
    /// The next pooled credential of `provider`, if it has a pool.
    fn pooled(&self, _provider: &str) -> Option<Pooled> {
        None
    }

    /// The usable subscription sign-in of `provider`, if any.
    ///
    /// # Errors
    ///
    /// Store errors.
    fn subscription(&self, _provider: &str) -> Result<Option<SignedIn>, Error> {
        Ok(None)
    }

    /// Providers with a sign-in, in sign-in order.
    fn signed_in(&self) -> Vec<String> {
        Vec::new()
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
    /// A pooled credential.
    Pool {
        /// Provider id.
        provider: String,
        /// 1-based pool position.
        index: usize,
        /// Label, if any.
        label: Option<String>,
    },
    /// A subscription sign-in.
    Subscription {
        /// Provider id.
        provider: String,
        /// The sign-in's label.
        label: String,
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

    /// The credential of subscription sign-in `signed_in` of `provider`.
    #[must_use]
    pub fn subscription(provider: &str, signed_in: SignedIn) -> Self {
        let source = CredentialSource::Subscription {
            provider: provider.to_owned(),
            label: signed_in.label,
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

    /// A subscription sign-in (sent as a bearer token, with the
    /// provider's subscription request identity).
    #[must_use]
    pub fn is_subscription(&self) -> bool {
        matches!(self.source, CredentialSource::Subscription { .. })
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

    /// `flag`, `pool:<provider>#<n>`, `subscription:<provider>/<label>`,
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
}

/// Resolves the credential of `provider` for a request to `base_url`;
/// `explicit` names a secret replacing the provider's own names. Between
/// a subscription sign-in and an API key, `providers.<id>.prefer_credential`
/// decides (subscription by default) unless the sign-in's plan was found
/// ineligible; either is used alone when it is the only one.
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
    let mut unbound = Vec::new();
    let mut subscription = None;
    if explicit.is_none() {
        if let Some(pooled) = resolver.extras.pooled(id) {
            let source = CredentialSource::Pool {
                provider: id.to_owned(),
                index: pooled.index,
                label: pooled.label,
            };
            return Ok(Credential::new(source, Some(pooled.value)));
        }
        subscription = bound_subscription(resolver, provider, &host, &mut unbound)?;
    }
    let prefer_subscription =
        provider.settings().prefer_credential == PreferCredential::Subscription;
    if let Some(signed_in) = subscription.take_if(|s| prefer_subscription && !s.ineligible) {
        return Ok(Credential::subscription(id, signed_in));
    }
    let names: Vec<String> =
        explicit.map_or_else(|| provider.secret_names(), |n| vec![n.to_owned()]);
    if let Some(key) = bound_secret(resolver, provider, &host, &names, &mut unbound)? {
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
    Err(missing(id, &host, &names, &unbound))
}

/// The first of `names` that is stored and bound to `host`.
fn bound_secret(
    resolver: &Resolver<'_>,
    provider: &Provider,
    host: &str,
    names: &[String],
    unbound: &mut Vec<String>,
) -> Result<Option<Credential>, Error> {
    for name in names {
        let Some(value) = resolver.secrets.secret(name)? else {
            continue;
        };
        let hosts = bound_hosts(resolver, provider, name);
        if hosts.iter().any(|pattern| host_matches(pattern, host)) {
            let source = CredentialSource::Secret { name: name.clone() };
            return Ok(Some(Credential::new(source, Some(value))));
        }
        unbound.push(format!("{name} is bound to {}", hosts.join(", ")));
    }
    Ok(None)
}

/// The provider's subscription sign-in when `host` is one of the
/// provider's own key hosts (a sign-in token is bound like a key).
fn bound_subscription(
    resolver: &Resolver<'_>,
    provider: &Provider,
    host: &str,
    unbound: &mut Vec<String>,
) -> Result<Option<SignedIn>, Error> {
    let Some(signed_in) = resolver.extras.subscription(provider.id())? else {
        return Ok(None);
    };
    let hosts = provider.key_hosts();
    if hosts.iter().any(|pattern| host_matches(pattern, host)) {
        return Ok(Some(signed_in));
    }
    unbound.push(format!(
        "sign-in '{}' is bound to {}",
        signed_in.label,
        hosts.join(", ")
    ));
    Ok(None)
}

/// The hosts secret `name` may be sent to on behalf of `provider`.
fn bound_hosts(resolver: &Resolver<'_>, provider: &Provider, name: &str) -> Vec<String> {
    let mut hosts: Vec<String> = resolver
        .registry
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

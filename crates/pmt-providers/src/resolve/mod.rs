//! Route resolution (spec: model-providers, Requested provider precedence
//! without ambient environment; Deterministic explainable resolution; Auto
//! provider detection; Local endpoints never fall back to remote
//! providers).
//!
//! One ordered procedure turns a requested provider and model into a
//! [`Route`]: provider lookup with the disabled guard, endpoint, wire
//! format, credential, model. Every step is recorded in a trace that
//! `prometheus model explain` prints; no step reads the process
//! environment, and nothing here sends a model request.

mod auto;
mod credential;
mod probe;

use pmt_config::Config;
use pmt_config::schema::{ModelConfig, WireFormat};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde::Serialize;

pub use auto::NOT_CONNECTED;
pub use credential::{
    Credential, CredentialExtras, CredentialSource, NoExtras, Pooled, SecretSource, SignedIn,
};
pub use probe::{BoxFuture, EndpointProbe, TcpProbe};

use crate::classify::RetryPolicy;
use crate::registry::{Provider, ProviderRegistry};
use crate::transport::Timeouts;
use crate::wire::{self, WireSource};

/// Where a requested value came from, strongest first (the order is the
/// precedence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestSource {
    /// `--provider` / `--model` or an API argument.
    Flag,
    /// `/model --session` or `/model --once`.
    Session,
    /// `model.provider` / `model.default`.
    Config,
    /// Nothing requested: automatic detection or the provider default.
    Auto,
}

/// A provider and/or model named at one precedence level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    /// Provider id or alias.
    pub provider: Option<String>,
    /// Model id.
    pub model: Option<String>,
}

/// What the caller asked for, above the profile config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Command-line flags or API arguments.
    pub flag: Layer,
    /// The session override.
    pub session: Layer,
}

/// The requested provider and model after precedence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requested {
    /// Provider id or alias, or `auto`.
    pub provider: String,
    /// Where it came from.
    pub provider_source: RequestSource,
    /// Model id, when one was requested.
    pub model: Option<String>,
    /// Where the model came from.
    pub model_source: RequestSource,
}

/// Applies the precedence flag > session override > profile config >
/// `auto`. A model is taken from a level only when that level names no
/// provider or the chosen one, so `--provider vllm` never inherits a
/// model configured for another provider.
#[must_use]
pub fn requested(selection: &Selection, model: &ModelConfig) -> Requested {
    let config = Layer {
        provider: model.provider.clone(),
        model: model.default.clone(),
    };
    let levels = [
        (RequestSource::Flag, &selection.flag),
        (RequestSource::Session, &selection.session),
        (RequestSource::Config, &config),
    ];
    let (provider, provider_source) = levels
        .iter()
        .find_map(|(source, layer)| layer.provider.clone().map(|p| (p, *source)))
        .unwrap_or_else(|| ("auto".to_owned(), RequestSource::Auto));
    let (model, model_source) = levels
        .iter()
        .find_map(|(source, layer)| {
            let compatible = layer.provider.as_ref().is_none_or(|p| *p == provider);
            layer
                .model
                .clone()
                .filter(|_| compatible)
                .map(|m| (Some(m), *source))
        })
        .unwrap_or((None, RequestSource::Auto));
    Requested {
        provider,
        provider_source,
        model,
        model_source,
    }
}

/// A resolved route: everything a model call needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Canonical provider id.
    pub provider: String,
    /// Model id.
    pub model: String,
    /// Wire format.
    pub wire_format: WireFormat,
    /// Why that format.
    pub wire_source: WireSource,
    /// Endpoint base URL.
    pub base_url: String,
    /// The credential (or none).
    pub credential: Credential,
    /// The provider honours a prompt cache key.
    pub supports_prompt_cache: bool,
    /// A local inference server.
    pub local: bool,
    /// Same-route retries.
    pub retry: RetryPolicy,
    /// Time limits.
    pub timeouts: Timeouts,
}

impl Route {
    /// `provider/model`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// The identity used to de-duplicate routes: canonical provider,
    /// lower-cased model, base URL without trailing slash.
    #[must_use]
    pub fn identity(&self) -> (String, String, String) {
        identity(&self.provider, &self.model, &self.base_url)
    }

    /// The ledger's `billing_source` of calls on this route.
    #[must_use]
    pub fn billing_source(&self) -> String {
        self.credential.billing_source(&self.provider, self.local)
    }
}

/// See [`Route::identity`].
#[must_use]
pub fn identity(provider: &str, model: &str, base_url: &str) -> (String, String, String) {
    (
        provider.to_owned(),
        model.to_lowercase(),
        base_url.trim_end_matches('/').to_owned(),
    )
}

/// The result of one resolution step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The step yielded.
    Ok,
    /// The step failed; resolution stopped.
    Failed,
}

/// One recorded step (never holds a secret value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    /// `requested`, `alias`, `auto`, `provider`, `enabled`, `endpoint`,
    /// `wire_format`, `credential`, `model`, `capability`.
    pub step: &'static str,
    /// Outcome.
    pub outcome: Outcome,
    /// What happened.
    pub detail: String,
}

/// A trace of steps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Trace(pub Vec<Step>);

impl Trace {
    /// Records a yielding step.
    pub fn ok(&mut self, step: &'static str, detail: impl Into<String>) {
        self.0.push(Step {
            step,
            outcome: Outcome::Ok,
            detail: detail.into(),
        });
    }

    /// Records `result`: the error message on failure.
    ///
    /// # Errors
    ///
    /// `result`'s error, unchanged.
    pub fn check<T>(&mut self, step: &'static str, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(err) = &result {
            self.0.push(Step {
                step,
                outcome: Outcome::Failed,
                detail: err.message().to_owned(),
            });
        }
        result
    }
}

/// A route target before resolution: one fallback entry, `[models]` route
/// or the requested provider and model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    /// Provider id or alias.
    pub provider: String,
    /// Model id (`None`: the provider default).
    pub model: Option<String>,
    /// Base URL replacing the provider's.
    pub base_url: Option<String>,
    /// Secret name replacing the provider's.
    pub credential: Option<String>,
}

/// Resolves routes for one profile.
pub struct Resolver<'a> {
    /// The profile's providers.
    pub registry: &'a ProviderRegistry,
    /// The profile's config.
    pub config: &'a Config,
    /// The profile's secrets.
    pub secrets: &'a dyn SecretSource,
    /// Pools and sign-ins.
    pub extras: &'a dyn CredentialExtras,
    /// Local endpoint reachability.
    pub probe: &'a dyn EndpointProbe,
    /// A launch-only endpoint (`--base-url`, `--api-key-stdin`).
    pub ad_hoc: Option<AdHoc>,
}

/// A launch-only endpoint (spec: model-providers, Launch-only ad-hoc
/// endpoint): the base URL replaces the requested provider's endpoint for
/// this launch, and the key (read from stdin, held in memory only) is sent
/// to that URL's host and nowhere else, so fallback routes never get it.
#[derive(Debug, Clone)]
pub struct AdHoc {
    base_url: String,
    host: String,
    key: Option<SecretString>,
}

impl AdHoc {
    /// An ad-hoc endpoint at `base_url` with an optional key.
    ///
    /// # Errors
    ///
    /// `config.invalid_argument` (exit 2) for a URL that is not `http(s)`,
    /// or plain HTTP to a host that is neither loopback nor private.
    pub fn new(base_url: &str, key: Option<SecretString>) -> Result<Self, Error> {
        use pmt_config::provider_urls::UrlProblem;
        let problem = |p| {
            let why = match p {
                UrlProblem::Malformed => "is not an http:// or https:// URL".to_owned(),
                UrlProblem::InsecurePublic { host } => {
                    format!("sends plain HTTP to the public host {host}; use https://")
                }
            };
            Error::new(
                &codes::CONFIG_INVALID_ARGUMENT,
                format!("--base-url '{base_url}' {why}"),
            )
        };
        pmt_config::provider_urls::check_url(base_url, false).map_err(problem)?;
        let host = pmt_config::provider_urls::host_of(base_url).unwrap_or_default();
        Ok(Self {
            base_url: base_url.to_owned(),
            host,
            key,
        })
    }

    /// The endpoint.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The key, when `base_url`'s host is the ad-hoc endpoint's host.
    #[must_use]
    pub fn key_for(&self, base_url: &str) -> Option<&SecretString> {
        let host = pmt_config::provider_urls::host_of(base_url)?;
        self.key
            .as_ref()
            .filter(|_| host.eq_ignore_ascii_case(&self.host))
    }
}

impl std::fmt::Debug for Resolver<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish_non_exhaustive()
    }
}

impl Resolver<'_> {
    /// Resolves `requested` (running auto detection for `auto`), recording
    /// each step in `trace`.
    ///
    /// # Errors
    ///
    /// The failing step's typed error: `provider.not_configured`,
    /// `provider.unknown`, `provider.disabled`, `provider.missing_base_url`,
    /// `provider.missing_credential`, `model.not_selected`.
    pub async fn resolve(&self, requested: &Requested, trace: &mut Trace) -> Result<Route, Error> {
        trace.ok(
            "requested",
            format!(
                "provider '{}' ({}), model {} ({})",
                requested.provider,
                source_name(requested.provider_source),
                requested
                    .model
                    .as_deref()
                    .map_or("<provider default>".to_owned(), |m| format!("'{m}'")),
                source_name(requested.model_source),
            ),
        );
        if let Some(target) = self.alias_target(requested) {
            trace.ok(
                "alias",
                format!(
                    "'{}' -> {}/{}",
                    requested.model.as_deref().unwrap_or_default(),
                    target.provider,
                    target.model.as_deref().unwrap_or_default()
                ),
            );
            return self.resolve_target(&self.with_ad_hoc(target), trace).await;
        }
        let provider = if self.ad_hoc.is_some() && requested.provider_source != RequestSource::Flag
        {
            // `--base-url` without `--provider` names a custom endpoint.
            "custom".to_owned()
        } else if requested.provider == "auto" {
            let chosen = auto::detect(self).await;
            let chosen = trace.check("auto", chosen)?;
            trace.ok("auto", format!("selected '{}': {}", chosen.0, chosen.1));
            chosen.0
        } else {
            requested.provider.clone()
        };
        let target = Target {
            provider,
            model: requested.model.clone(),
            ..Target::default()
        };
        self.resolve_target(&self.with_ad_hoc(target), trace).await
    }

    /// The `[model.aliases]` route `requested.model` names, unless a
    /// provider was requested at a stronger level than the model (then the
    /// model id is taken literally).
    fn alias_target(&self, requested: &Requested) -> Option<Target> {
        let target = crate::target::alias(&self.config.model, requested.model.as_deref()?)?;
        (requested.provider_source >= requested.model_source).then_some(target)
    }

    /// `target` with the launch-only endpoint, when one was given.
    fn with_ad_hoc(&self, mut target: Target) -> Target {
        if let Some(ad_hoc) = &self.ad_hoc {
            target.base_url = Some(ad_hoc.base_url.clone());
        }
        target
    }

    /// Resolves one explicit target (fallback entries use this too).
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve), without `provider.not_configured`.
    pub async fn resolve_target(&self, target: &Target, trace: &mut Trace) -> Result<Route, Error> {
        let provider = trace.check("provider", self.registry.lookup(&target.provider))?;
        trace.ok(
            "provider",
            format!("'{}' -> '{}'", target.provider, provider.id()),
        );
        if !provider.enabled() {
            return trace.check("enabled", Err(crate::registry::disabled(provider.id())));
        }
        trace.ok(
            "enabled",
            format!("providers.{}.enabled = true", provider.id()),
        );
        if let Some(err) = wire::virtual_unavailable(provider) {
            return trace.check("endpoint", Err(err));
        }
        let base_url = trace.check(
            "endpoint",
            self.endpoint(provider, target.base_url.as_deref()).await,
        )?;
        trace.ok("endpoint", base_url.clone());
        let (wire_format, wire_source) = wire::select(provider, &base_url);
        trace.ok(
            "wire_format",
            format!(
                "{} (from {})",
                wire_format.as_str(),
                wire_source_name(wire_source)
            ),
        );
        trace.check(
            "wire_format",
            wire::available(provider, wire_source).map_or(Ok(()), Err),
        )?;
        let credential =
            credential::resolve(self, provider, &base_url, target.credential.as_deref());
        let credential = trace.check("credential", credential)?;
        trace.ok("credential", credential.describe());
        let model = target
            .model
            .clone()
            .or_else(|| provider.descriptor().default_model.clone())
            .ok_or_else(|| no_model(provider.id()));
        let model = trace.check("model", model)?;
        trace.ok("model", model.clone());
        let settings = provider.settings();
        Ok(Route {
            provider: provider.id().to_owned(),
            model,
            wire_format,
            wire_source,
            base_url,
            credential,
            supports_prompt_cache: provider.supports_prompt_cache(),
            local: provider.descriptor().local,
            retry: RetryPolicy::new(settings.max_retries),
            timeouts: Timeouts {
                request: std::time::Duration::from_secs(settings.request_timeout_secs),
                idle: std::time::Duration::from_secs(settings.stream_idle_timeout_secs),
            },
        })
    }

    /// The base URL: an explicit one, the configured one, else the
    /// descriptor default (probed for local servers, which never fall back
    /// to another provider).
    async fn endpoint(&self, provider: &Provider, explicit: Option<&str>) -> Result<String, Error> {
        if let Some(url) = explicit.or(provider.settings().base_url.as_deref()) {
            return Ok(url.to_owned());
        }
        let id = provider.id();
        let hint = format!("prometheus config set providers.{id}.base_url http://<host>:<port>/v1");
        let Some(default) = provider.descriptor().base_url.as_deref() else {
            return Err(Error::new(
                &codes::PROVIDER_MISSING_BASE_URL,
                format!("provider '{id}' has no endpoint: set providers.{id}.base_url"),
            )
            .with_hint(hint));
        };
        if provider.descriptor().local && !self.probe.reachable(default).await {
            return Err(Error::new(
                &codes::PROVIDER_MISSING_BASE_URL,
                format!(
                    "provider '{id}' has no providers.{id}.base_url and its default endpoint {default} is not reachable"
                ),
            )
            .with_hint(hint));
        }
        Ok(default.to_owned())
    }
}

fn no_model(provider: &str) -> Error {
    Error::new(
        &codes::MODEL_NOT_SELECTED,
        format!("no model selected for provider '{provider}' and it has no default model"),
    )
    .with_hint("pass --model <id> or run: prometheus config set model.default <id>")
}

fn source_name(source: RequestSource) -> &'static str {
    match source {
        RequestSource::Flag => "flag",
        RequestSource::Session => "session override",
        RequestSource::Config => "config",
        RequestSource::Auto => "auto",
    }
}

fn wire_source_name(source: WireSource) -> &'static str {
    match source {
        WireSource::Config => "config",
        WireSource::HostRule => "host rule",
        WireSource::Descriptor => "descriptor",
        WireSource::Default => "default",
    }
}

#[cfg(test)]
mod tests;

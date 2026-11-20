//! The profile side of the subscription proxy: the sign-in's token,
//! refreshed under the cross-process refresh lock when it is due, and the
//! usage ledger (one hidden session per proxy run, tagged `proxy`; every
//! call `purpose = "proxy"`, `billing_source = "subscription:<provider>"`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pmt_api::subscription_proxy::{ProxiedCall, ProxyHost};
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_providers::ProviderRegistry;
use pmt_providers::auth::Credentials;
use pmt_providers::auth::refresh::Refresher;
use pmt_store::{CostStatus, NewSession, Source, Store, Tokens, UsageEvent};

use crate::context::Context;

/// The session tag of a proxy run's ledger rows.
const SOURCE_TAG: &str = "proxy";

pub(super) struct ProfileHost {
    registry: ProviderRegistry,
    provider: String,
    label: String,
    credentials: Credentials,
    refresher: Refresher,
    store: Arc<Store>,
    session: SessionId,
}

impl ProfileHost {
    /// The host of `scope`'s sign-in `label` of `provider`.
    pub(super) fn open(
        scope: &ProfileScope,
        _ctx: &Context<'_>,
        provider: &str,
        label: &str,
    ) -> Result<Self, Error> {
        let registry = ProviderRegistry::for_profile(scope.config(), Vec::new())?;
        registry.lookup(provider)?;
        let store = Arc::new(Store::open(scope)?);
        let mut session = NewSession::new(Source::Api);
        session.source_tag = Some(SOURCE_TAG.to_owned());
        session.title = Some(format!("Subscription proxy ({provider})"));
        let session = store.create_session(&session)?.id;
        let client = crate::model_cmd::client(scope.config())?;
        Ok(Self {
            registry,
            provider: provider.to_owned(),
            label: label.to_owned(),
            credentials: Credentials::new(scope.secrets().store().clone()),
            refresher: Refresher::new(client, Refresher::lock_dir_of(scope.dir())),
            store,
            session,
        })
    }

    /// The provider's API base.
    pub(super) fn base_url(&self) -> String {
        self.registry
            .lookup(&self.provider)
            .ok()
            .and_then(|p| p.base_url())
            .unwrap_or_default()
            .to_owned()
    }
}

impl ProxyHost for ProfileHost {
    fn credential(&self) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let provider = self
                .registry
                .lookup(&self.provider)
                .map_err(|e| e.to_string())?;
            self.refresher
                .owned(&self.credentials, provider, &self.label, None)
                .await
                .map(|signed| signed.token.expose().to_owned())
                .map_err(|err| {
                    let why = Error::new(
                        &codes::PROXY_NOT_LOGGED_IN,
                        format!("the {} sign-in cannot be used: {err:?}", self.provider),
                    );
                    why.message().to_owned()
                })
        })
    }

    fn record(&self, call: ProxiedCall) {
        let event = UsageEvent {
            call_id: format!("proxy_{}", ulid::Ulid::generate()),
            ts: jiff::Timestamp::now(),
            session_id: self.session,
            turn_id: None,
            parent_call_id: None,
            purpose: SOURCE_TAG.to_owned(),
            purpose_detail: Some(call.path),
            provider: self.provider.clone(),
            model: call.model.unwrap_or_else(|| "unknown".to_owned()),
            service_tier: None,
            tokens: Tokens {
                input: call.input_tokens,
                output: call.output_tokens,
                ..Tokens::default()
            },
            cost_usd: None,
            cost_status: CostStatus::Subscription,
            billing_source: format!("subscription:{}", self.provider),
            request_hash: None,
            cache_hit: false,
        };
        if let Err(err) = self.store.record_usage(&event) {
            tracing::warn!(error = %err, "a proxied call was not recorded in the usage ledger");
        }
    }
}

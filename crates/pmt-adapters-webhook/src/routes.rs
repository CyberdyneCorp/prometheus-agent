//! The routes one webhook adapter serves: the configured
//! `[gateway.platforms.webhook.routes.<name>]` tables, fixed for the
//! adapter's life, and the profile's subscriptions (`prometheus webhooks
//! subscribe`, `pmt_config::webhook_subscriptions`), re-read whenever their
//! file changes so a new subscription is served without a restart (spec:
//! cli, Webhook subscriptions). A configured route wins over a
//! subscription of the same name; a subscription whose signing secret does
//! not resolve is not served (logged).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use pmt_config::ProfileScope;
use pmt_config::schema::WebhookRoute;
use pmt_config::webhook_subscriptions;
use pmt_gateway::messaging::adapter::credential;

/// The routes by name.
pub(crate) type RouteMap = BTreeMap<String, WebhookRoute>;

/// The served routes.
pub(crate) struct Routes {
    configured: RouteMap,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The subscriptions file's stamp when it was last read (`None`: no
    /// file); `read` is false until the first read.
    stamp: Option<(SystemTime, u64)>,
    read: bool,
    merged: Arc<RouteMap>,
    /// The signing secret of each served subscription.
    secrets: BTreeMap<String, String>,
}

impl Routes {
    /// The configured routes, before any subscription is read.
    pub(crate) fn new(configured: RouteMap) -> Self {
        Self {
            state: Mutex::new(State {
                merged: Arc::new(configured.clone()),
                ..State::default()
            }),
            configured,
        }
    }

    /// The configured routes only.
    pub(crate) fn configured(&self) -> &RouteMap {
        &self.configured
    }

    /// Every route served now, after re-reading the subscriptions when
    /// their file changed.
    pub(crate) fn current(&self, scope: &ProfileScope) -> Arc<RouteMap> {
        let stamp = webhook_subscriptions::stamp(scope.dir());
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.read || state.stamp != stamp {
            let (merged, secrets) = self.merge(scope);
            state.merged = Arc::new(merged);
            state.secrets = secrets;
            state.stamp = stamp;
            state.read = true;
        }
        Arc::clone(&state.merged)
    }

    /// The signing secret of subscribed route `name` (as last read).
    pub(crate) fn subscribed_secret(&self, name: &str) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.secrets.get(name).cloned()
    }

    fn merge(&self, scope: &ProfileScope) -> (RouteMap, BTreeMap<String, String>) {
        let mut merged = self.configured.clone();
        let mut secrets = BTreeMap::new();
        for (name, route) in webhook_subscriptions::routes(scope.dir()) {
            if merged.contains_key(&name) {
                tracing::warn!(route = %name, "a configured webhook route of this name wins over the subscription");
                continue;
            }
            let Some(secret) = &route.secret_secret else {
                tracing::warn!(route = %name, "webhook subscription without a secret is not served");
                continue;
            };
            match credential(scope, &secret.0) {
                Ok(value) => {
                    secrets.insert(name.clone(), value);
                    merged.insert(name, route);
                }
                Err(err) => {
                    tracing::warn!(route = %name, error = %err.message, "webhook subscription not served: its secret does not resolve");
                }
            }
        }
        (merged, secrets)
    }
}

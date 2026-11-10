//! `prometheus status --deep` (spec: observability, Configuration-centric
//! status): the only status mode that makes outbound requests. One
//! authenticated model-listing request per provider with a credential,
//! a connection to every endpoint each enabled adapter declares
//! (`pmt_gateway::messaging::probe`; an adapter that only receives is
//! `inbound only`), and an MCP `initialize` per configured server, each
//! within [`WITHIN`], reporting latency or the error.

use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_providers::resolve::{Target, Trace};
use serde::Serialize;

use crate::context::Context;

/// Every probe's deadline.
pub(super) const WITHIN: Duration = Duration::from_secs(10);

/// One probe's outcome.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Probe {
    /// `provider`, `adapter` or `mcp`.
    pub kind: &'static str,
    /// What was probed (`openai`, `telegram`, `github`).
    pub name: String,
    /// Milliseconds to the answer, when it answered.
    pub latency_ms: Option<u64>,
    /// Why it failed.
    pub error: Option<String>,
    /// Nothing to probe: the adapter only receives.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inbound_only: bool,
}

impl Probe {
    fn of(kind: &'static str, name: &str, outcome: Result<Duration, String>) -> Self {
        let (latency_ms, error) = match outcome {
            Ok(took) => (
                Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX)),
                None,
            ),
            Err(why) => (None, Some(why)),
        };
        Self {
            kind,
            name: name.to_owned(),
            latency_ms,
            error,
            inbound_only: false,
        }
    }
}

/// Runs every probe of the active profile.
pub(super) fn probes(ctx: &Context<'_>, scope: &ProfileScope) -> Vec<Probe> {
    let mut probes = providers(ctx, scope);
    probes.extend(adapters(ctx, scope));
    probes.extend(mcp(scope));
    probes
}

fn providers(ctx: &Context<'_>, scope: &ProfileScope) -> Vec<Probe> {
    let Ok(models) = crate::model_cmd::Models::open(ctx) else {
        return Vec::new();
    };
    let Ok(client) = crate::model_cmd::client(scope.config()) else {
        return Vec::new();
    };
    let configured: Vec<String> = crate::setup_cmd::sections::required_secrets(scope)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.present)
        .map(|s| s.provider)
        .collect();
    let resolver = models.resolver(scope.config(), None);
    let probe = async {
        let mut out = Vec::new();
        for id in &configured {
            out.push(Probe::of(
                "provider",
                id,
                provider(&models, &resolver, &client, id).await,
            ));
        }
        out
    };
    crate::model_cmd::block_on(probe).unwrap_or_default()
}

async fn provider(
    models: &crate::model_cmd::Models,
    resolver: &pmt_providers::resolve::Resolver<'_>,
    client: &reqwest::Client,
    id: &str,
) -> Result<Duration, String> {
    let entry = models
        .registry
        .lookup(id)
        .map_err(|e| e.message().to_owned())?;
    let url = entry
        .models_url()
        .ok_or("no model listing endpoint to probe")?;
    let target = Target {
        provider: id.to_owned(),
        model: configured_model(models.scope.config(), entry.id()),
        ..Target::default()
    };
    let route = resolver
        .resolve_target(&target, &mut Trace::default())
        .await
        .map_err(|e| e.message().to_owned())?;
    pmt_providers::probe::models(client, &route, &url, WITHIN).await
}

/// The model the profile uses with provider `id` (`model.default`, or its
/// fallback entry's), so the probe resolves the route a call would take.
fn configured_model(config: &pmt_config::Config, id: &str) -> Option<String> {
    let main = config.model.provider.as_deref().is_some_and(|p| p == id);
    if main {
        return config.model.default.clone();
    }
    config
        .fallback
        .chain
        .iter()
        .find(|e| e.provider == id)
        .map(|e| e.model.clone())
}

#[cfg(feature = "gateway")]
fn adapters(ctx: &Context<'_>, scope: &ProfileScope) -> Vec<Probe> {
    let factories = crate::daemon_cmd::adapter_factories(ctx);
    let probe = async {
        let mut out = Vec::new();
        for (name, table) in &scope.config().gateway.platforms {
            if table.enabled {
                let setup = pmt_gateway::messaging::AdapterSetup {
                    scope: scope.clone(),
                    platform: name.clone(),
                    config: table.clone(),
                };
                out.push(adapter(&factories, &setup).await);
            }
        }
        out
    };
    crate::model_cmd::block_on(probe).unwrap_or_default()
}

/// Builds the adapter `setup` describes (no connection) and probes the
/// endpoints it declares.
#[cfg(feature = "gateway")]
async fn adapter(
    factories: &std::collections::BTreeMap<String, pmt_gateway::messaging::AdapterFactory>,
    setup: &pmt_gateway::messaging::AdapterSetup,
) -> Probe {
    use pmt_gateway::messaging::probe::{self, Probed};
    let name = setup.platform.as_str();
    let reach = factories
        .get(name)
        .ok_or_else(|| format!("this build has no adapter for platform '{name}'"))
        .and_then(|factory| factory(setup).map_err(|e| e.message().to_owned()))
        .and_then(|adapter| adapter.reach());
    let outcome = match reach {
        Ok(reach) => probe::probe(setup, &reach, WITHIN).await,
        Err(why) => Err(why),
    };
    match outcome {
        Ok(Probed::InboundOnly) => Probe {
            kind: "adapter",
            name: name.to_owned(),
            latency_ms: None,
            error: None,
            inbound_only: true,
        },
        Ok(Probed::Reached(took)) => Probe::of("adapter", name, Ok(took)),
        Err(why) => Probe::of("adapter", name, Err(why)),
    }
}

/// Without the messaging gateway no adapter can start, so none is probed.
#[cfg(not(feature = "gateway"))]
fn adapters(_ctx: &Context<'_>, _scope: &ProfileScope) -> Vec<Probe> {
    Vec::new()
}

fn mcp(scope: &ProfileScope) -> Vec<Probe> {
    let manager = crate::mcp_host::manager(scope);
    let probe = async {
        let mut out = Vec::new();
        for server in manager.servers() {
            let started = Instant::now();
            let outcome = match tokio::time::timeout(WITHIN, server.connect()).await {
                Ok(Ok(())) => Ok(started.elapsed()),
                Ok(Err(why)) => Err(why),
                Err(_) => Err(format!("no initialize answer within {}s", WITHIN.as_secs())),
            };
            server.stop();
            out.push(Probe::of("mcp", server.name(), outcome));
        }
        out
    };
    crate::model_cmd::block_on(probe).unwrap_or_default()
}

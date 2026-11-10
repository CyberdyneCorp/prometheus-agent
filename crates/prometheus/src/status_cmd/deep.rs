//! `prometheus status --deep` (spec: observability, Configuration-centric
//! status): the only status mode that makes outbound requests. One
//! authenticated model-listing request per provider with a credential,
//! one connection per enabled adapter's platform, and an MCP `initialize`
//! per configured server, each within [`WITHIN`], reporting latency or
//! the error.

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
        }
    }
}

/// Runs every probe of the active profile.
pub(super) fn probes(ctx: &Context<'_>, scope: &ProfileScope) -> Vec<Probe> {
    let mut probes = providers(ctx, scope);
    probes.extend(adapters(scope));
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

/// The API host an adapter of `platform` connects to, when fixed.
fn platform_host(platform: &str) -> Option<&'static str> {
    match platform {
        "telegram" => Some("api.telegram.org:443"),
        "discord" => Some("discord.com:443"),
        "slack" => Some("slack.com:443"),
        "whatsapp" => Some("graph.facebook.com:443"),
        _ => None,
    }
}

fn adapters(scope: &ProfileScope) -> Vec<Probe> {
    scope
        .config()
        .gateway
        .platforms
        .iter()
        .filter(|(_, p)| p.enabled)
        .map(|(name, _)| {
            let outcome = platform_host(name).map_or_else(
                || Err("no fixed platform endpoint; see the daemon's adapter state".to_owned()),
                connect,
            );
            Probe::of("adapter", name, outcome)
        })
        .collect()
}

/// A TCP connection to `host:port` within [`WITHIN`].
fn connect(host: &str) -> Result<Duration, String> {
    let started = Instant::now();
    let addr = std::net::ToSocketAddrs::to_socket_addrs(host)
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("cannot resolve {host}"))?;
    std::net::TcpStream::connect_timeout(&addr, WITHIN).map_err(|e| format!("{host}: {e}"))?;
    Ok(started.elapsed())
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

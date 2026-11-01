//! Shared setup: a profile config pointing providers at mock servers, a
//! resolver over in-memory secrets, and request helpers.

use std::collections::BTreeMap;
use std::time::Duration;

use pmt_config::Config;
use pmt_core::secret::SecretString;
use pmt_providers::metadata::Catalog;
use pmt_providers::request::{Message, Request, Role, SystemBlock, Tool};
use pmt_providers::resolve::{NoExtras, Resolver, Target, TcpProbe, Trace};
use pmt_providers::{ModelCaller, ProviderRegistry, Route};
use wiremock::ResponseTemplate;

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};
static EXTRAS: NoExtras = NoExtras;

/// One profile's provider setup.
pub(crate) struct Env {
    pub config: Config,
    pub registry: ProviderRegistry,
    pub secrets: BTreeMap<String, SecretString>,
    pub catalog: Catalog,
}

impl Env {
    pub(crate) fn new(config: &str, secrets: &[(&str, &str)]) -> Self {
        let config: Config = toml::from_str(config).unwrap();
        pmt_config::provider_urls::check(&config).unwrap();
        let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
        let catalog = Catalog::new(&config, None);
        Self {
            secrets: secrets
                .iter()
                .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
                .collect(),
            config,
            registry,
            catalog,
        }
    }

    pub(crate) fn resolver(&self) -> Resolver<'_> {
        Resolver {
            registry: &self.registry,
            config: &self.config,
            secrets: &self.secrets,
            extras: &EXTRAS,
            probe: &PROBE,
            ad_hoc: None,
        }
    }

    /// The route of `provider`/`model`, with millisecond retry backoff.
    pub(crate) async fn route(&self, provider: &str, model: &str) -> Route {
        let target = Target {
            provider: provider.to_owned(),
            model: Some(model.to_owned()),
            ..Target::default()
        };
        let mut route = self
            .resolver()
            .resolve_target(&target, &mut Trace::default())
            .await
            .unwrap();
        fast(&mut route);
        route
    }
}

pub(crate) fn fast(route: &mut Route) {
    route.retry.base = Duration::from_millis(1);
    route.retry.cap = Duration::from_millis(5);
}

pub(crate) fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/providers/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).unwrap()
}

/// A streamed response with a fixture body.
pub(crate) fn sse(name: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(fixture(name))
}

pub(crate) fn caller() -> ModelCaller {
    ModelCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap())
}

/// A request with a frozen two-tier system prompt and one tool.
pub(crate) fn request(user: &str) -> Request {
    Request {
        model: String::new(),
        system: vec![
            SystemBlock {
                text: "You are a careful assistant.".into(),
                cache_breakpoint: true,
            },
            SystemBlock {
                text: "Project: demo.".into(),
                cache_breakpoint: true,
            },
        ],
        messages: vec![Message::text(Role::User, user)],
        tools: vec![Tool {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
        max_output_tokens: Some(1024),
        cache: pmt_providers::request::CacheControl::default(),
        reasoning: None,
    }
}

/// The JSON bodies the server received on `path_suffix`.
pub(crate) async fn bodies(
    server: &wiremock::MockServer,
    path_suffix: &str,
) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path().ends_with(path_suffix))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

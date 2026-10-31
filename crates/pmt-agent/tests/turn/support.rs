//! A profile in a temp home, a session, and an agent over the built-in
//! mock provider (scripted per test) with recorded requests and events.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, ProviderModel, ProviderSetup,
    RequestedCall, RouteInfo,
};
use pmt_agent::turn::tools::{ToolOutcome, ToolPort};
use pmt_agent::turn::{
    Agent, AgentSetup, LeaseTiming, PromptSetup, Recorded, TurnInput, TurnOptions, TurnResult,
};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_obs::trajectory::events::Event;
use pmt_obs::usage::{Prices, UsageRecorder};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::metadata::Catalog;
use pmt_providers::mock::{Mock, Script};
use pmt_providers::request::{Request, Tool};
use pmt_providers::resolve::{NoExtras, SecretSource, Target, TcpProbe};
use pmt_providers::{ModelCaller, ProviderRegistry};
use pmt_store::{NewSession, Source, Store};
use pmt_tools::ToolResult;
use serde_json::json;

/// The config every test profile starts from.
pub(crate) const BASE: &str = "[model]\nprovider = \"mock\"\ndefault = \"mock\"\n";

/// A temp home with one profile.
pub(crate) struct Profile {
    pub(crate) root: Arc<tempfile::TempDir>,
    pub(crate) home: Home,
    pub(crate) scope: ProfileScope,
    pub(crate) store: Arc<Store>,
}

impl Profile {
    /// Profile `name` with `extra` appended to [`BASE`].
    pub(crate) fn new(extra: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        Self::open_in(Arc::new(root), home, "default", extra)
    }

    fn open_in(root: Arc<tempfile::TempDir>, home: Home, name: &str, extra: &str) -> Self {
        let dir = home.profile(&ProfileName::parse(name).unwrap()).unwrap();
        dir.config_file()
            .replace(&format!("{BASE}{extra}"))
            .unwrap();
        let scope = ProfileScope::open(dir, &Overrides::parse::<&str>(&[]).unwrap(), Launch::Other)
            .unwrap();
        let store = Arc::new(Store::open(&scope).unwrap());
        Self {
            root,
            home,
            scope,
            store,
        }
    }

    /// Another profile of the same home.
    pub(crate) fn sibling(&self, name: &str, extra: &str) -> Self {
        Self::open_in(self.root.clone(), self.home.clone(), name, extra)
    }

    /// Rewrites the profile's config (`extra` after [`BASE`]) and reopens
    /// the scope, as a new session would see it.
    pub(crate) fn reconfigure(&mut self, extra: &str) {
        let dir = self.scope.profile().clone();
        dir.config_file()
            .replace(&format!("{BASE}{extra}"))
            .unwrap();
        self.scope =
            ProfileScope::open(dir, &Overrides::parse::<&str>(&[]).unwrap(), Launch::Other)
                .unwrap();
    }

    /// The profile directory.
    pub(crate) fn dir(&self) -> &Path {
        self.scope.dir()
    }

    /// A directory under the temp root.
    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        let path = self.root.path().join(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A new session.
    pub(crate) fn session(&self, source: Source) -> SessionId {
        self.store
            .create_session(&NewSession::new(source))
            .unwrap()
            .id
    }
}

/// Records every request, then delegates.
pub(crate) struct Recording {
    inner: Arc<dyn ModelPort>,
    requests: Mutex<Vec<Request>>,
    unpriced: bool,
}

impl Recording {
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ModelPort for Recording {
    fn route(&self) -> BoxFuture<'_, RouteInfo> {
        Box::pin(async move {
            let mut info = self.inner.route().await;
            if self.unpriced {
                info.price = None;
            }
            info
        })
    }

    fn start_turn(&self) -> BoxFuture<'_, ()> {
        self.inner.start_turn()
    }

    fn call<'a>(
        &'a self,
        request: &'a Request,
        ctx: &'a CallContext,
        deltas: &'a dyn DeltaSink,
    ) -> BoxFuture<'a, Result<ModelReply, ModelFailure>> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        self.inner.call(request, ctx, deltas)
    }
}

/// The mock provider serving `script` for `model`.
pub(crate) async fn mock_model(profile: &Profile, script: &str, model: &str) -> Arc<Recording> {
    mock_model_with(profile, script, model, false).await
}

/// [`mock_model`], optionally reporting the model as unpriced.
pub(crate) async fn mock_model_with(
    profile: &Profile,
    script: &str,
    model: &str,
    unpriced: bool,
) -> Arc<Recording> {
    let config = Arc::new(profile.scope.config().clone());
    let registry = Arc::new(ProviderRegistry::for_profile(&config, Vec::new()).unwrap());
    let catalog = Arc::new(Catalog::new(&config, None));
    let secrets: Arc<dyn SecretSource> = Arc::new(BTreeMap::<String, SecretString>::new());
    let prices: Arc<dyn Prices> = catalog.clone();
    let mock = Arc::new(Mock::new(Script::parse(script, "test.toml").unwrap()));
    let caller = ModelCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap())
        .with_mock(mock)
        .with_attempt_cap(config.agent.api_max_retries)
        .with_recorder(Arc::new(UsageRecorder::new(profile.store.clone(), prices)));
    let setup = ProviderSetup {
        config,
        registry,
        secrets,
        extras: Arc::new(NoExtras),
        probe: Arc::new(TcpProbe {
            timeout: Duration::from_millis(200),
        }),
        catalog,
        health: Arc::new(Healthy),
        ad_hoc: None,
    };
    let target = Target {
        provider: "mock".into(),
        model: Some(model.into()),
        ..Target::default()
    };
    let inner = ProviderModel::start(setup, caller, &target).await.unwrap();
    Arc::new(Recording {
        inner: Arc::new(inner),
        requests: Mutex::new(Vec::new()),
        unpriced,
    })
}

/// A tool `lookup` answering `result of <arguments>`, after `delay`, and
/// running `effect` on each call.
pub(crate) struct FakeTools {
    pub(crate) delay: Duration,
    pub(crate) output: String,
    pub(crate) calls: Mutex<Vec<RequestedCall>>,
    #[allow(clippy::type_complexity)]
    pub(crate) effect: Option<Box<dyn Fn() + Send + Sync>>,
}

impl FakeTools {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::plain())
    }

    /// Answers at once with nothing extra.
    pub(crate) fn plain() -> Self {
        Self {
            delay: Duration::ZERO,
            output: String::new(),
            calls: Mutex::new(Vec::new()),
            effect: None,
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl ToolPort for FakeTools {
    fn definitions(&self) -> Vec<Tool> {
        vec![Tool {
            name: "lookup".into(),
            description: "Look something up.".into(),
            parameters: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        }]
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        cancel: &'a CancelToken,
    ) -> BoxFuture<'a, Vec<ToolOutcome>> {
        Box::pin(async move {
            if let Some(effect) = &self.effect {
                effect();
            }
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(calls.iter().cloned());
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {}
                () = cancel.cancelled() => {}
            }
            calls
                .iter()
                .map(|c| ToolOutcome {
                    result: ToolResult::ok(format!("result of {}{}", c.arguments, self.output)),
                    duration_ms: 1,
                })
                .collect()
        })
    }
}

/// An agent of `session` with recorded events.
pub(crate) struct Run {
    pub(crate) agent: Agent,
    pub(crate) events: Arc<Recorded>,
    pub(crate) session: SessionId,
}

/// Builds the agent; `tweak` adjusts the setup.
pub(crate) fn agent(
    profile: &Profile,
    session: SessionId,
    model: Arc<Recording>,
    tools: Arc<dyn ToolPort>,
    tweak: impl FnOnce(&mut AgentSetup),
) -> Run {
    let events = Recorded::new();
    let mut setup = AgentSetup::new(
        profile.scope.clone(),
        profile.store.clone(),
        session,
        model,
        tools,
        PromptSetup::new("cli", None),
    );
    setup.events = events.clone();
    tweak(&mut setup);
    Run {
        agent: Agent::new(setup).unwrap(),
        events,
        session,
    }
}

/// Fast lease timing for tests.
pub(crate) fn options() -> TurnOptions {
    TurnOptions {
        lease: LeaseTiming {
            ttl: Duration::from_secs(5),
            renew_every: Duration::from_millis(100),
            admission_wait: Duration::from_millis(300),
        },
        ..TurnOptions::default()
    }
}

impl Run {
    /// One turn with `text`.
    pub(crate) async fn turn(&self, profile: &Profile, text: &str) -> TurnResult {
        self.turn_with(profile, TurnInput::text(text), options())
            .await
    }

    pub(crate) async fn turn_with(
        &self,
        profile: &Profile,
        input: TurnInput,
        options: TurnOptions,
    ) -> TurnResult {
        self.agent
            .run_turn(Some(&profile.scope), self.session, input, options)
            .await
            .unwrap()
    }

    /// The kinds of every recorded event.
    pub(crate) fn kinds(&self) -> Vec<&'static str> {
        self.events.events().iter().map(Event::kind).collect()
    }

    /// The `turn.end` reasons.
    pub(crate) fn end_reasons(&self) -> Vec<String> {
        self.events
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::TurnEnd(end) => Some(end.reason),
                _ => None,
            })
            .collect()
    }
}

/// The system text of `request` (all blocks).
pub(crate) fn system_text(request: &Request) -> String {
    request
        .system
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

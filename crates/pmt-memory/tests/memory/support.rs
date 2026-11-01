//! A profile in a temp home, its store, a session's memory and an agent
//! over the scripted mock provider whose only tool is `memory`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_agent::tools::MemoryPort;
use pmt_agent::turn::model::{
    BoxFuture, DeltaSink, ModelFailure, ModelPort, ModelReply, ProviderModel, ProviderSetup,
    RequestedCall, RouteInfo,
};
use pmt_agent::turn::tools::{ToolBatch, ToolOutcome, ToolPort};
use pmt_agent::turn::{
    Agent, AgentSetup, PromptSetup, Recorded, TurnInput, TurnOptions, TurnResult,
};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_memory::{MemorySession, MemorySetup};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::metadata::Catalog;
use pmt_providers::mock::{Mock, Script};
use pmt_providers::request::{Request, Tool};
use pmt_providers::resolve::{NoExtras, SecretSource, Target, TcpProbe};
use pmt_providers::{ModelCaller, ProviderRegistry};
use pmt_store::{NewSession, Source, Store};
use serde_json::{Value, json};

const BASE: &str = "[model]\nprovider = \"mock\"\ndefault = \"mock\"\n";

pub(crate) struct Profile {
    pub(crate) _root: tempfile::TempDir,
    pub(crate) scope: ProfileScope,
    pub(crate) store: Arc<Store>,
}

impl Profile {
    /// A profile whose config is [`BASE`] plus `extra`.
    pub(crate) fn new(extra: &str) -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        let dir = home
            .profile(&ProfileName::parse("default").unwrap())
            .unwrap();
        dir.config_file()
            .replace(&format!("{BASE}{extra}"))
            .unwrap();
        let scope = ProfileScope::open(dir, &Overrides::parse::<&str>(&[]).unwrap(), Launch::Other)
            .unwrap();
        let store = Arc::new(Store::open(&scope).unwrap());
        Self {
            _root: root,
            scope,
            store,
        }
    }

    pub(crate) fn session(&self) -> SessionId {
        self.store
            .create_session(&NewSession::new(Source::Cli))
            .unwrap()
            .id
    }

    pub(crate) fn setup(&self, session: SessionId) -> MemorySetup {
        MemorySetup::new(
            self.scope.clone(),
            self.store.clone(),
            Some(session),
            Source::Cli,
        )
    }

    pub(crate) fn memory(&self, session: SessionId) -> Arc<MemorySession> {
        Arc::new(MemorySession::new(self.setup(session)))
    }
}

/// Runs one `memory` call and parses its JSON result.
pub(crate) async fn call(memory: &MemorySession, args: Value) -> (bool, Value) {
    let result = memory.call(args).await;
    let ok = matches!(result.status, pmt_tools::Status::Ok);
    (ok, serde_json::from_str(&result.text_content()).unwrap())
}

/// Adds `content` to `target`'s core tier and returns the id.
pub(crate) async fn add(memory: &MemorySession, target: &str, content: &str) -> String {
    let (ok, out) = call(
        memory,
        json!({"target": target, "operations": [{"action": "add", "content": content}]}),
    )
    .await;
    assert!(ok, "{out}");
    out["results"][0]["id"].as_str().unwrap().to_owned()
}

/// Records every request, then delegates.
pub(crate) struct Recording {
    inner: Arc<dyn ModelPort>,
    requests: Mutex<Vec<Request>>,
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
        self.inner.route()
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

/// The mock provider serving `script`.
pub(crate) async fn mock_model(profile: &Profile, script: &str) -> Arc<Recording> {
    let config = Arc::new(profile.scope.config().clone());
    let registry = Arc::new(ProviderRegistry::for_profile(&config, Vec::new()).unwrap());
    let catalog = Arc::new(Catalog::new(&config, None));
    let secrets: Arc<dyn SecretSource> = Arc::new(BTreeMap::<String, SecretString>::new());
    let mock = Arc::new(Mock::new(Script::parse(script, "test.toml").unwrap()));
    let caller = ModelCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap())
        .with_mock(mock);
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
        model: Some("mock".into()),
        ..Target::default()
    };
    let inner = ProviderModel::start(setup, caller, &target).await.unwrap();
    Arc::new(Recording {
        inner: Arc::new(inner),
        requests: Mutex::new(Vec::new()),
    })
}

/// `memory` as the session's only tool, run by the session's memory.
pub(crate) struct MemoryTools {
    pub(crate) memory: Arc<MemorySession>,
    pub(crate) definition: Tool,
}

impl ToolPort for MemoryTools {
    fn definitions(&self) -> Vec<Tool> {
        vec![self.definition.clone()]
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        _: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        Box::pin(async move {
            let mut outcomes = Vec::new();
            for call in calls {
                let arguments = serde_json::from_str(&call.arguments).unwrap();
                outcomes.push(ToolOutcome {
                    result: self.memory.call(arguments).await,
                    duration_ms: 0,
                });
            }
            ToolBatch {
                outcomes,
                plan: None,
            }
        })
    }
}

/// An agent for `session` wired as session start wires memory: the
/// snapshot in the prompt setup, the refresher and the memory port.
pub(crate) struct Run {
    pub(crate) agent: Agent,
    pub(crate) events: Arc<Recorded>,
    pub(crate) session: SessionId,
    pub(crate) memory: Arc<MemorySession>,
}

pub(crate) fn agent(profile: &Profile, session: SessionId, model: Arc<dyn ModelPort>) -> Run {
    let memory = profile.memory(session);
    let tools = Arc::new(MemoryTools {
        memory: memory.clone(),
        definition: pmt_memory::tool::definition(profile.scope.config()),
    });
    let mut prompt = PromptSetup::new("cli", None);
    memory.refresh(&mut prompt);
    let mut setup = AgentSetup::new(
        profile.scope.clone(),
        profile.store.clone(),
        session,
        model,
        tools,
        prompt,
    );
    let events = Recorded::new();
    setup.events = events.clone();
    let refresher = memory.clone();
    setup.refresh_prompt = Some(Arc::new(move |p: &mut PromptSetup| refresher.refresh(p)));
    setup.memory = Some(memory.clone());
    Run {
        agent: Agent::new(setup).unwrap(),
        events,
        session,
        memory,
    }
}

impl Run {
    pub(crate) async fn turn(&self, profile: &Profile, text: &str) -> TurnResult {
        self.agent
            .run_turn(
                Some(&profile.scope),
                self.session,
                TurnInput::text(text),
                TurnOptions::default(),
            )
            .await
            .unwrap()
    }
}

/// The text of every system block of `request`.
pub(crate) fn system_text(request: &Request) -> String {
    request
        .system
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

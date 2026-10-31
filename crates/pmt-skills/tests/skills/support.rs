//! A profile in a temp home with skills, a session's `SkillsSession` and
//! a dispatcher over the `skills` toolset, as a session builds them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::model::{ProviderModel, ProviderSetup};
use pmt_agent::turn::tools::{SessionTools, Timings};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::capability::{BASELINE, CapabilitySet};
use pmt_core::id::SessionId;
use pmt_core::panic::CrashReporter;
use pmt_core::secret::{Redactor, SecretString};
use pmt_obs::usage::{Prices, UsageRecorder};
use pmt_providers::fallback::Healthy;
use pmt_providers::metadata::Catalog as ModelCatalog;
use pmt_providers::mock::{Mock, Script};
use pmt_providers::resolve::{NoExtras, SecretSource, Target, TcpProbe};
use pmt_providers::{ModelCaller, ProviderRegistry};
use pmt_skills::bundled::Bundled;
use pmt_skills::session::{Attachment, SessionSetup, SkillSessions, SkillsSession};
use pmt_store::{NewSession, Source, Store};
use pmt_tools::budget::ResultBudget;
use pmt_tools::dispatch::{AllowAll, NoHooks};
use pmt_tools::probe::ProbeCache;
use pmt_tools::spillover::session_dir;
use pmt_tools::{
    Builtins, DispatchSetup, Dispatcher, Selection, SurfaceInputs, ToolCall, ToolRegistry,
    ToolResult, ToolSurface,
};
use serde_json::Value;

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
    /// Profile `default` with `extra` config after [`BASE`].
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
        let scope = open_scope(&dir);
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
        Self::open_in(Arc::clone(&self.root), self.home.clone(), name, extra)
    }

    /// Rewrites the config and reopens the scope (a new session's view).
    pub(crate) fn reconfigure(&mut self, extra: &str) {
        let dir = self.scope.profile().clone();
        dir.config_file()
            .replace(&format!("{BASE}{extra}"))
            .unwrap();
        self.scope = open_scope(&dir);
    }

    /// `<profile>/skills`.
    pub(crate) fn skills(&self) -> PathBuf {
        self.scope.dir().join("skills")
    }

    /// A directory under the temp root (created).
    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        let path = self.root.path().join(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A new session row.
    pub(crate) fn session(&self) -> SessionId {
        self.store
            .create_session(&NewSession::new(Source::Cli))
            .unwrap()
            .id
    }

    /// A session setup with an empty bundled tree.
    pub(crate) fn setup(&self, session: SessionId) -> SessionSetup {
        let mut setup = SessionSetup::new(
            self.scope.clone(),
            Arc::clone(&self.store),
            session.to_string(),
        );
        setup.bundled = Arc::new(Bundled::default());
        setup.user_home = Some(self.path("user-home"));
        setup
    }
}

fn open_scope(dir: &pmt_config::ProfileDir) -> ProfileScope {
    ProfileScope::open(
        dir.clone(),
        &Overrides::parse::<&str>(&[]).unwrap(),
        Launch::Other,
    )
    .unwrap()
}

/// Writes `text` to `dir/rel` (parents created).
pub(crate) fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A `SKILL.md` text.
pub(crate) fn skill_md(name: &str, description: &str, extra: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n{extra}---\n\n# {name}\n\nSteps.\n")
}

/// Writes skill `rel` (its directory relative to `root`).
pub(crate) fn add_skill(root: &Path, rel: &str, name: &str, description: &str) {
    write(
        root,
        &format!("{rel}/SKILL.md"),
        &skill_md(name, description, ""),
    );
}

/// Copies the agentskills.io fixture tree into `dest`.
pub(crate) fn copy_fixtures(dest: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agentskills");
    copy_dir(&src, dest);
}

fn copy_dir(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let target = dest.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A session's skills attached to a private table, and its dispatcher.
pub(crate) struct Session {
    pub(crate) id: SessionId,
    pub(crate) skills: Arc<SkillsSession>,
    pub(crate) sessions: Arc<SkillSessions>,
    pub(crate) dispatcher: Arc<Dispatcher>,
    _attached: Attachment,
}

impl Session {
    /// Builds the session's skills from `setup` and a dispatcher over the
    /// `skills` toolset.
    pub(crate) async fn start(profile: &Profile, setup: SessionSetup) -> Self {
        let id: SessionId = setup.session_id.parse().unwrap();
        let sessions = Arc::new(SkillSessions::default());
        let skills = Arc::new(SkillsSession::new(setup));
        let attached = sessions.attach(&id.to_string(), Arc::clone(&skills));
        let dispatcher = Arc::new(dispatcher(profile, &sessions, &id.to_string()).await);
        Self {
            id,
            skills,
            sessions,
            dispatcher,
            _attached: attached,
        }
    }

    /// A session with the default setup.
    pub(crate) async fn open(profile: &Profile) -> Self {
        let id = profile.session();
        Self::start(profile, profile.setup(id)).await
    }

    /// Calls `tool` with `args`.
    pub(crate) async fn call(&self, tool: &str, args: Value) -> ToolResult {
        self.dispatcher
            .dispatch(
                ToolCall::new("call_1", tool, &args.to_string()),
                &CancelToken::new(),
            )
            .await
    }

    /// The text of a call.
    pub(crate) async fn text(&self, tool: &str, args: Value) -> String {
        self.call(tool, args).await.text_content()
    }

    /// The JSON of a successful call.
    pub(crate) async fn json(&self, tool: &str, args: Value) -> Value {
        let result = self.call(tool, args).await;
        assert!(result.status.succeeded(), "{}", result.text_content());
        serde_json::from_str(&result.text_content()).unwrap()
    }

    /// The agent's tool port.
    pub(crate) fn tool_port(&self) -> Arc<SessionTools> {
        Arc::new(SessionTools::new(
            None,
            Arc::clone(&self.dispatcher),
            Arc::new(Timings::default()),
        ))
    }
}

/// A dispatcher of session `id` over the `skills` toolset.
pub(crate) async fn dispatcher(
    profile: &Profile,
    sessions: &Arc<SkillSessions>,
    id: &str,
) -> Dispatcher {
    let config = profile.scope.config();
    let tools = pmt_skills::tools::tools(sessions);
    let registry = ToolRegistry::new("default", Arc::new(Builtins::standard(tools).unwrap()));
    let selection = Selection::resolve(&registry, &["skills".to_owned()], &[]).unwrap();
    let capabilities = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
    let surface = ToolSurface::build(SurfaceInputs {
        registry: &registry,
        selection: &selection,
        capabilities: &capabilities,
        config,
        probes: &ProbeCache::new(),
    })
    .await;
    Dispatcher::new(DispatchSetup {
        session_id: id.to_owned(),
        surface: Arc::new(surface),
        capabilities: Arc::new(capabilities),
        hooks: Arc::new(NoHooks),
        gate: Arc::new(AllowAll),
        observer: Arc::new(Timings::default()),
        budget: ResultBudget::new(
            config.tools.result_budget.clone(),
            None,
            session_dir(profile.scope.dir(), id),
            Redactor::default(),
        ),
        reporter: Arc::new(CrashReporter::new(
            profile.scope.dir().join("crash"),
            Redactor::default(),
        )),
        default_timeout: Duration::from_mins(5),
        concurrency: 8,
    })
}

/// The mock provider serving `script`.
pub(crate) async fn mock_model(profile: &Profile, script: &str) -> Arc<ProviderModel> {
    let config = Arc::new(profile.scope.config().clone());
    let registry = Arc::new(ProviderRegistry::for_profile(&config, Vec::new()).unwrap());
    let catalog = Arc::new(ModelCatalog::new(&config, None));
    let secrets: Arc<dyn SecretSource> = Arc::new(BTreeMap::<String, SecretString>::new());
    let prices: Arc<dyn Prices> = catalog.clone();
    let mock = Arc::new(Mock::new(Script::parse(script, "test.toml").unwrap()));
    let caller = ModelCaller::new(pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap())
        .with_mock(mock)
        .with_recorder(Arc::new(UsageRecorder::new(
            Arc::clone(&profile.store),
            prices,
        )));
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
    Arc::new(ProviderModel::start(setup, caller, &target).await.unwrap())
}

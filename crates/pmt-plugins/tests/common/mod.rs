//! A profile with WASM plugins installed, enabled and approved, and ports
//! that record what reaches them.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_config::grants::GrantsFile;
use pmt_config::schema::PluginsConfig;
use pmt_core::capability::CapabilitySet;
use pmt_plugins::grants::{Identity, approve};
use pmt_plugins::host::ports::{
    ActionError, ConfigPort, EventsPort, HttpPort, HttpRequest, HttpResponse, InjectTarget,
    LlmCall, LlmPort, MessagingPort, PlatformAction, PortFuture, Ports, PostFailure, Posted,
    SecretsPort, SubagentAnswer, SubagentFailure, SubagentRequest, SubagentsPort, ToolsPort,
};
use pmt_plugins::host::{CallInfo, Instance, Runtime};
use pmt_plugins::load::{LoadInputs, Loaded, load};
use pmt_plugins::manifest::Manifest;
use pmt_plugins::testkit;
use semver::Version;

/// Everything the fake ports saw.
#[derive(Default)]
pub(crate) struct Seen {
    pub http: Mutex<Vec<String>>,
    pub llm: Mutex<Vec<LlmCall>>,
    pub events: Mutex<Vec<String>>,
    /// `post <session> <text>`, `edit ...`, `inject ...`, `action <verb> ...`.
    pub messaging: Mutex<Vec<String>>,
    /// The `subagents` requests, with their session.
    pub subagents: Mutex<Vec<(String, SubagentRequest)>>,
}

/// Ports backed by memory.
pub(crate) struct Fake {
    pub seen: Arc<Seen>,
    pub secrets: BTreeMap<String, String>,
    pub settings: Mutex<BTreeMap<(String, String), serde_json::Value>>,
}

impl Fake {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Arc::new(Seen::default()),
            secrets: [("OPENAI_API_KEY", "sk-x"), ("NOTION_API_KEY", "secret_n")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            settings: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn set(&self, plugin: &str, key: &str, value: &str) {
        self.settings.lock().unwrap().insert(
            (plugin.to_owned(), key.to_owned()),
            serde_json::from_str(value).unwrap(),
        );
    }

    pub(crate) fn ports(self: &Arc<Self>) -> Ports {
        Ports {
            http: self.clone(),
            secrets: self.clone(),
            llm: self.clone(),
            tools: self.clone(),
            events: self.clone(),
            config: self.clone(),
            messaging: self.clone(),
            subagents: self.clone(),
        }
    }
}

/// Launches answer the handle `h-<session>`; only that handle is known.
impl SubagentsPort for Fake {
    fn call<'a>(
        &'a self,
        session: &'a str,
        _: &'a str,
        request: SubagentRequest,
    ) -> PortFuture<'a, Result<SubagentAnswer, SubagentFailure>> {
        self.seen
            .subagents
            .lock()
            .unwrap()
            .push((session.to_owned(), request.clone()));
        let known = |handle: &str| handle == format!("h-{session}");
        let answer = match request {
            SubagentRequest::Launch { .. } => Ok(SubagentAnswer::Handle(format!("h-{session}"))),
            SubagentRequest::Status { handle } if known(&handle) => {
                Ok(SubagentAnswer::State("running"))
            }
            SubagentRequest::Wait { handle, .. } if known(&handle) => Ok(SubagentAnswer::Result(
                Some(r#"{"status":"completed"}"#.to_owned()),
            )),
            SubagentRequest::Steer { handle, .. } | SubagentRequest::Cancel { handle }
                if known(&handle) =>
            {
                Ok(SubagentAnswer::Done)
            }
            _ => Err(SubagentFailure::UnknownHandle),
        };
        Box::pin(async move { answer })
    }
}

impl MessagingPort for Fake {
    fn post(&self, session_id: &str, text: &str) -> PortFuture<'_, Result<Posted, PostFailure>> {
        self.seen
            .messaging
            .lock()
            .unwrap()
            .push(format!("post {session_id} {text}"));
        Box::pin(async {
            Ok(Posted {
                handle: "m1".to_owned(),
                fallback: false,
            })
        })
    }

    fn edit(
        &self,
        session_id: &str,
        handle: &str,
        text: &str,
    ) -> PortFuture<'_, Result<Posted, PostFailure>> {
        self.seen
            .messaging
            .lock()
            .unwrap()
            .push(format!("edit {session_id} {handle} {text}"));
        let handle = handle.to_owned();
        Box::pin(async move {
            Ok(Posted {
                handle,
                fallback: false,
            })
        })
    }

    fn inject(&self, target: &InjectTarget, content: &str) -> PortFuture<'_, Result<(), String>> {
        self.seen
            .messaging
            .lock()
            .unwrap()
            .push(format!("inject {target:?} {content}"));
        Box::pin(async { Ok(()) })
    }

    fn platform_action(&self, action: &PlatformAction) -> PortFuture<'_, Result<(), ActionError>> {
        self.seen
            .messaging
            .lock()
            .unwrap()
            .push(format!("action {} {}", action.verb, action.platform));
        Box::pin(async { Ok(()) })
    }
}

impl HttpPort for Fake {
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        self.seen.http.lock().unwrap().push(request.url);
        Box::pin(async {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: b"sunny".to_vec(),
            })
        })
    }
}

impl SecretsPort for Fake {
    fn get(&self, name: &str) -> Option<String> {
        self.secrets.get(name).cloned()
    }
}

impl LlmPort for Fake {
    fn complete(&self, call: LlmCall) -> PortFuture<'_, Result<String, String>> {
        self.seen.llm.lock().unwrap().push(call);
        Box::pin(async { Ok("model says hi".to_owned()) })
    }
}

impl ToolsPort for Fake {
    fn dispatch(
        &self,
        _: Option<String>,
        name: String,
        _: String,
    ) -> PortFuture<'_, Result<String, String>> {
        Box::pin(async move { Ok(format!("ran {name}")) })
    }
}

impl EventsPort for Fake {
    fn emit(&self, _: &str, name: &str, _: &str) {
        self.seen.events.lock().unwrap().push(name.to_owned());
    }

    fn subscribe(&self, _: &str, _: &str, _: &str) {}
}

impl ConfigPort for Fake {
    fn get(&self, plugin: &str, key: &str) -> Option<serde_json::Value> {
        self.settings
            .lock()
            .unwrap()
            .get(&(plugin.to_owned(), key.to_owned()))
            .cloned()
    }

    fn set(&self, plugin: &str, key: &str, value: serde_json::Value) -> Result<(), String> {
        self.settings
            .lock()
            .unwrap()
            .insert((plugin.to_owned(), key.to_owned()), value);
        Ok(())
    }
}

/// A profile directory under `target/`.
pub(crate) struct Profile {
    pub dir: tempfile::TempDir,
    pub name: String,
    pub config: PluginsConfig,
    pub blocked: Vec<pmt_plugins::catalog::Removed>,
}

pub(crate) fn version() -> Version {
    Version::new(0, 1, 0)
}

impl Profile {
    pub(crate) fn new(name: &str) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-homes");
        std::fs::create_dir_all(&base).unwrap();
        Self {
            dir: tempfile::tempdir_in(base).unwrap(),
            name: name.to_owned(),
            config: PluginsConfig::default(),
            blocked: Vec::new(),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    pub(crate) fn plugin_dir(&self, id: &str) -> PathBuf {
        self.path().join("plugins").join(id)
    }

    /// Installs crate `component` as plugin `id` with `[capabilities]`
    /// text and `worlds`, enables it and approves everything it asks for.
    pub(crate) fn install(
        &mut self,
        id: &str,
        component: &str,
        worlds: &str,
        capabilities: &str,
    ) -> Identity {
        self.install_with(id, component, worlds, capabilities, "")
    }

    /// [`Self::install`] with `top` added to the manifest's top-level keys
    /// (`requires_plugins = [...]`).
    pub(crate) fn install_with(
        &mut self,
        id: &str,
        component: &str,
        worlds: &str,
        capabilities: &str,
        top: &str,
    ) -> Identity {
        let identity = self.place_with(id, component, worlds, capabilities, top);
        self.config.enabled.push(id.to_owned());
        let manifest = Manifest::read(&self.plugin_dir(id), &version()).unwrap();
        let requested: CapabilitySet = manifest
            .capabilities
            .requested(&self.path().join("plugin-data").join(id))
            .unwrap();
        approve(&self.grants(), &identity, &requested, "test").unwrap();
        identity
    }

    /// Writes the plugin without enabling or approving it.
    pub(crate) fn place(
        &self,
        id: &str,
        component: &str,
        worlds: &str,
        capabilities: &str,
    ) -> Identity {
        self.place_with(id, component, worlds, capabilities, "")
    }

    /// [`Self::place`] with `top` added to the manifest's top-level keys.
    pub(crate) fn place_with(
        &self,
        id: &str,
        component: &str,
        worlds: &str,
        capabilities: &str,
        top: &str,
    ) -> Identity {
        let dir = self.plugin_dir(id);
        testkit::install(component, &dir);
        let manifest = format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [{worlds}]\n{top}\n\n[capabilities]\n{capabilities}\n"
        );
        std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
        let digest = pmt_plugins::digest::of_dir(&dir).unwrap();
        adopted(&dir, &digest);
        Identity {
            id: id.to_owned(),
            digest,
        }
    }

    pub(crate) fn grants(&self) -> GrantsFile {
        GrantsFile::at(self.path().join("grants.toml"))
    }

    pub(crate) fn load(&self, runtime: &Runtime, ports: &Ports) -> Loaded {
        load(
            runtime,
            ports,
            &LoadInputs {
                profile: &self.name,
                profile_dir: self.path(),
                cwd: None,
                config: &self.config,
                embedded: &[],
                running: &version(),
                blocked: &self.blocked,
                profile_config: None,
            },
        )
    }
}

/// Writes the install record of an adopted directory.
pub(crate) fn adopted(dir: &Path, digest: &str) {
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: digest.to_owned(),
        signer: None,
        tier: None,
        installed_at: "2026-11-03T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(dir)
    .unwrap();
}

/// The loaded instance of `id`, failing the test with its status otherwise.
pub(crate) fn instance(loaded: &Loaded, id: &str) -> Arc<Instance> {
    loaded
        .plugins
        .iter()
        .find(|p| p.manifest.id == id)
        .map_or_else(
            || panic!("{id} not loaded: {:?}", loaded.statuses),
            |p| Arc::clone(&p.instance),
        )
}

/// Calls tool `name` of `instance` with `{"v": v}` on a blocking thread.
pub(crate) async fn call(
    instance: &Arc<Instance>,
    name: &str,
    v: &str,
) -> Result<Result<String, String>, pmt_core::error::Error> {
    let instance = Arc::clone(instance);
    let name = name.to_owned();
    let args = format!(r#"{{"v":"{v}"}}"#);
    tokio::task::spawn_blocking(move || {
        let call = CallInfo {
            session_id: Some("ses_test".to_owned()),
            surface: "cli".to_owned(),
            cwd: None,
        };
        instance.call_tool(&call, &name, &args)
    })
    .await
    .unwrap()
}

pub(crate) const TOOL: &str = "\"tool@1.0\"";
pub(crate) const HOOK: &str = "\"hook@1.1\"";
pub(crate) const PROVIDER: &str = "\"provider@1.0\"";

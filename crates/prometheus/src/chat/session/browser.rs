//! The browser tools of chat sessions (spec: execution-environments,
//! Browser session resolution, `browser_exec` tool; distribution-and-update,
//! Lazy runtime installation policy; Persistent runtime opt-outs).
//!
//! One [`BrowserManager`] serves every session of the process (it reaps
//! crashed owners' browsers when created); each session gets a
//! [`BrowserExec`] on its own sandbox and scratch directory. Chromium and
//! CPython come from the managed runtime store under `daemon.toml`'s
//! `[runtimes]` policy: a missing runtime is installed on first use when
//! lazy installs are allowed (asking first on an interactive session) and
//! never when it is excluded.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use pmt_config::{ProfileScope, SharedScope};
use pmt_core::capability::CapabilitySet;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_core::secret::SecretString;
use pmt_net::DestinationPolicy;
use pmt_net::resolve::SystemLookup;
use pmt_sandbox::Sandbox;
use pmt_tools::browser::cloud::{self, TokenSource};
use pmt_tools::browser::options::Display;
use pmt_tools::browser::real_profile::HostDetect;
use pmt_tools::browser::{
    BrowserExec, BrowserManager, BrowserSetup, ExecSetup, Request, RuntimePolicy, UrlSafety,
};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use pmt_update::runtimes::lazy::Confirm;
use tokio::sync::{mpsc, oneshot};

use crate::runtimes_cmd::browser::for_home;

/// A runtime install question for the person (`Install <name> <version>
/// (<size>)? [Y/n]`), answered by the interactive surface.
#[derive(Debug)]
pub struct InstallAsk {
    /// The question.
    pub question: String,
    /// The answer: `true` installs.
    pub reply: oneshot::Sender<bool>,
}

/// What every session's browser tools share.
pub struct Shared {
    manager: Arc<BrowserManager>,
    python: RuntimePolicy,
}

impl Shared {
    /// The profile's browser manager and runtime policies; `asks` receives
    /// the install questions of an interactive surface (`None` installs
    /// without asking, as a non-interactive session does).
    ///
    /// # Errors
    ///
    /// An invalid `daemon.toml` or `security.egress`, or no HTTP client.
    pub fn new(
        scope: &ProfileScope,
        current: &SharedScope,
        launch: &LaunchEnv,
        redactor: &Redactor,
        asks: Option<mpsc::UnboundedSender<InstallAsk>>,
    ) -> Result<Self, Error> {
        let config = scope.config();
        let policies = for_home(
            scope.home(),
            &config.network,
            asks.map(confirm),
            launch.inherited("PATH"),
        )?;
        let egress = &config.security.egress;
        let destinations = DestinationPolicy::new(egress.allow_private, &egress.fake_ip_ranges)?;
        let providers = cloud_providers(scope)?;
        // `browser.inactivity_timeout_secs` is live: read at each use.
        let live = current.clone();
        let manager = BrowserManager::new(BrowserSetup {
            config: Arc::new(move || live.get().config().browser.clone()),
            runtime: policies.chromium,
            env: ChildEnv::build(launch, EnvRequest::default()),
            cache_dir: scope.path("cache/browser"),
            safety: UrlSafety {
                destinations,
                lookup: Arc::new(SystemLookup),
                secrets: redactor.clone(),
                egress: Some(Arc::new(crate::egress::session_policy)),
                session: None,
            },
            providers,
            confinement: pmt_tools::browser::launch::Confinement {
                user_home: launch.user_home().cloned().unwrap_or_default(),
                prometheus_home: scope.home().to_path_buf(),
                sandbox: config.security.sandbox.clone(),
                launcher: None,
            },
            profile_dir: scope.dir().to_path_buf(),
            display: Display::of(launch),
            detect: Arc::new(HostDetect {
                user_home: launch.user_home().cloned().unwrap_or_default(),
                path_var: launch.inherited("PATH").map(ToOwned::to_owned),
            }),
        });
        Ok(Self {
            manager,
            python: policies.python,
        })
    }

    /// The profile's browser sessions (`/browser`).
    #[must_use]
    pub fn manager(&self) -> &Arc<BrowserManager> {
        &self.manager
    }

    /// `browser_exec` of one session: its interpreter runs under `sandbox`
    /// with `env`, its workspaces live in `<scratch>/browser`, its browsers
    /// see its `granted` capabilities (`browser.real_profile`).
    #[must_use]
    pub fn exec(
        &self,
        sandbox: Arc<Sandbox>,
        env: ChildEnv,
        scratch: &Path,
        image_input: bool,
        granted: &CapabilitySet,
    ) -> BrowserExec {
        BrowserExec::new(ExecSetup {
            manager: Arc::clone(&self.manager),
            python: self.python.clone(),
            sandbox,
            env,
            workspace_root: scratch.join("browser"),
            image_input,
            request: Request {
                granted: granted.clone(),
            },
        })
    }
}

/// The provider whose sign-in serves the `subscription` cloud browsers.
const SUBSCRIPTION_PROVIDER: &str = "nous";

/// The first-party cloud browser providers of `scope` (spec:
/// execution-environments, First-party cloud browser providers): their
/// keys come from the profile's secrets store and the `subscription`
/// provider's token from its Nous sign-in, read at each use.
fn cloud_providers(
    scope: &ProfileScope,
) -> Result<BTreeMap<String, Arc<dyn pmt_tools::browser::CloudProvider>>, Error> {
    let config = scope.config();
    let policy = pmt_net::OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
        proxy: config.network.proxy.clone(),
    };
    let client = pmt_net::client(&policy)?;
    let secrets = scope.secrets().clone();
    let lookup: pmt_tools::web::SecretLookup =
        Arc::new(move |name: &str| secrets.resolve(name).ok());
    let credentials = pmt_providers::auth::Credentials::new(scope.secrets().store().clone());
    let token: TokenSource = Arc::new(move || {
        pmt_providers::resolve::CredentialExtras::subscription(&credentials, SUBSCRIPTION_PROVIDER)
            .ok()
            .flatten()
            .map(|signed_in| SecretString::new(signed_in.token.expose()))
    });
    Ok(cloud::first_party(&client, &lookup, token))
}

/// Install questions sent to the interactive surface; no answer (the
/// surface went away) declines.
fn confirm(asks: mpsc::UnboundedSender<InstallAsk>) -> Box<Confirm> {
    Box::new(move |question| {
        let asks = asks.clone();
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if asks.send(InstallAsk { question, reply }).is_err() {
                return false;
            }
            answer.await.unwrap_or(false)
        })
    })
}

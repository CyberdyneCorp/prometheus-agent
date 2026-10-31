//! The session's local execution environment (spec: execution-
//! environments, tool-system): the file tools' backend, the terminal on
//! the local sandbox and the web tools, attached to the process-wide
//! table the built-in tools look their session up in.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::capability::{CapabilitySet, Places};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_net::OutboundPolicy;
use pmt_sandbox::{Paths, Policy, Sandbox};
use pmt_tools::environments::{Attachment, Environments, SessionEnv};
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use pmt_tools::terminal::guard::GuardChain;
use pmt_tools::terminal::{LocalSetup, LocalTerminal, spill};
use pmt_tools::web::{FullTextStore, WebSession};

/// Where the session's environment lives.
pub struct EnvInputs<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// The session id.
    pub session_id: &'a str,
    /// The launch environment.
    pub launch: &'a LaunchEnv,
    /// The session's working directory.
    pub cwd: &'a Path,
    /// The session's effective capabilities.
    pub capabilities: &'a CapabilitySet,
    /// Known secret values to keep out of tool output.
    pub redactor: Redactor,
}

/// Builds and attaches the environment of one session; the returned
/// attachment detaches it when dropped. The terminal is left out (with a
/// WARN) when no shell or sandbox can be set up, so the other tools still
/// work.
///
/// # Errors
///
/// The scratch directory cannot be created.
pub fn attach(inputs: &EnvInputs<'_>) -> Result<Attachment, Error> {
    let scope = inputs.scope;
    let scratch = scope.path("cache/scratch").join(inputs.session_id);
    std::fs::create_dir_all(&scratch).map_err(|e| {
        Error::new(
            &pmt_core::error::codes::HOME_IO,
            format!("cannot create {}: {e}", scratch.display()),
        )
    })?;
    let user_home = inputs.launch.user_home().cloned();
    let places = Places {
        user_home: user_home.clone().unwrap_or_default(),
        workspace: inputs.cwd.to_path_buf(),
        scratch: vec![scratch.clone()],
        toolchain: Vec::new(),
    };
    let env = SessionEnv {
        files: Some(Arc::new(files(inputs, places, user_home.clone()))),
        terminal: terminal(inputs, &scratch, user_home)
            .inspect_err(|err| tracing::warn!(error = %err, "terminal unavailable this session"))
            .ok()
            .map(Arc::new),
        web: web(scope).map(Arc::new),
        vision: None,
        browser: None,
    };
    Ok(Environments::process().attach(inputs.session_id, env))
}

fn files(inputs: &EnvInputs<'_>, places: Places, user_home: Option<PathBuf>) -> FileSession {
    let cwd = inputs.cwd.to_path_buf();
    FileSession::new(FileSetup {
        backend: Arc::new(LocalBackend::new(inputs.capabilities.clone(), places)),
        cwd: Arc::new(move || cwd.clone()),
        user_home,
        control: ControlPaths {
            prometheus_home: inputs.scope.home().to_path_buf(),
        },
        read_max_chars: 100_000,
        linter: Linter::builtin(),
    })
}

fn terminal(
    inputs: &EnvInputs<'_>,
    scratch: &Path,
    user_home: Option<PathBuf>,
) -> Result<LocalTerminal, Error> {
    let config = inputs.scope.config();
    let paths = Paths {
        user_home: user_home.clone().unwrap_or_default(),
        prometheus_home: inputs.scope.home().to_path_buf(),
        workspace: inputs.cwd.to_path_buf(),
        scratch: vec![scratch.to_path_buf()],
    };
    let policy = Policy::from_set(inputs.capabilities, &paths);
    let sandbox = Arc::new(Sandbox::new(policy, &config.security.sandbox)?);
    let mut env = ChildEnv::build(
        inputs.launch,
        EnvRequest {
            passthrough: &config.terminal.env_passthrough,
            ..EnvRequest::default()
        },
    );
    env.set_temp_dir(scratch);
    let bundle = scratch.join("ca-bundle.pem");
    let extra_ca = config.network.extra_ca_file.as_deref().map(Path::new);
    if pmt_net::tls::write_child_bundle(&bundle, extra_ca).is_ok() {
        env.set_ca_bundle(&bundle);
    }
    let shell = pmt_tools::terminal::doctor::local_shell(&config.terminal, inputs.launch)?;
    LocalTerminal::new(LocalSetup {
        terminal: config.terminal.clone(),
        max_chars: 50_000,
        kill_grace: Duration::from_secs(2),
        sandbox,
        env,
        shell,
        scratch: scratch.to_path_buf(),
        spill_dir: spill::session_dir(inputs.scope.dir(), inputs.session_id),
        start_cwd: inputs.cwd.to_path_buf(),
        user_home,
        redactor: inputs.redactor.clone(),
        hooks: Vec::new(),
        guards: GuardChain::default(),
        notices: Vec::new(),
        notify: None,
        clock: None,
    })
    .map_err(|e| {
        Error::new(
            &pmt_core::error::codes::HOME_IO,
            format!("cannot set up the terminal: {e}"),
        )
    })
}

/// The web tools, when the profile can build an outbound client.
fn web(scope: &ProfileScope) -> Option<WebSession> {
    let config = scope.config();
    let policy = OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
    };
    let client = pmt_net::client(&policy).ok()?;
    let egress = &config.security.egress;
    let destinations =
        pmt_net::destination::DestinationPolicy::new(egress.allow_private, &egress.fake_ip_ranges)
            .ok()?;
    let fetcher = Arc::new(pmt_net::fetch::Fetcher::new(&policy, destinations).ok()?);
    let secrets = scope.secrets().clone();
    let lookup: pmt_tools::web::SecretLookup =
        Arc::new(move |name: &str| secrets.resolve(name).ok());
    Some(WebSession::from_config(
        &config.web,
        &client,
        fetcher,
        &lookup,
        FullTextStore::for_profile(scope.dir()),
    ))
}

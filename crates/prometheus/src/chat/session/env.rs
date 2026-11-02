//! The session's local execution environment (spec: execution-
//! environments, tool-system): the file tools' backend, the terminal on
//! the local sandbox, the web tools and `browser_exec`, attached to the
//! process-wide table the built-in tools look their session up in.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::{ProfileScope, SharedScope};
use pmt_core::capability::{CapabilitySet, Places};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_net::OutboundPolicy;
use pmt_sandbox::{Paths, Policy, Sandbox};
use pmt_tools::browser::BrowserExec;
use pmt_tools::environments::{Attachment, Environments, SessionEnv};
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use pmt_tools::terminal::guard::GuardChain;
use pmt_tools::terminal::shell::Shell;
use pmt_tools::terminal::{LocalSetup, LocalTerminal, spill};
use pmt_tools::web::{FullTextStore, WebSession};

use super::browser::Shared;

/// Where the session's environment lives.
pub struct EnvInputs<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// The session's configuration as it is now (its `live` keys follow
    /// reloads).
    pub config: &'a SharedScope,
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
    /// The process's browser tools, when they could be set up.
    pub browser: Option<&'a Shared>,
    /// The session's model accepts images (browser screenshots).
    pub image_input: bool,
}

/// An attached environment.
pub struct Attached {
    /// Detaches the environment when dropped.
    pub attachment: Attachment,
    /// The session's `browser_exec` (its browsers close at session end).
    pub browser: Option<Arc<BrowserExec>>,
    /// The session's terminal (skills pass their declared secrets to it).
    pub terminal: Option<Arc<LocalTerminal>>,
    /// How the session runs a command: its sandbox, child environment and
    /// shell (skills' inline snippets run the same way).
    pub commands: Option<Commands>,
}

/// The parts a session runs commands with.
#[derive(Clone)]
pub struct Commands {
    /// The OS sandbox.
    pub sandbox: Arc<Sandbox>,
    /// The child environment.
    pub env: ChildEnv,
    /// The shell.
    pub shell: Shell,
}

/// Builds and attaches the environment of one session; the returned
/// attachment detaches it when dropped. The terminal and the browser,
/// which runs its interpreter on the terminal's sandbox, are left out
/// (with a WARN) when no shell or sandbox can be set up, so the other
/// tools still work.
///
/// # Errors
///
/// The scratch directory cannot be created.
pub fn attach(inputs: &EnvInputs<'_>) -> Result<Attached, Error> {
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
    let paths = Paths {
        user_home: user_home.clone().unwrap_or_default(),
        prometheus_home: inputs.scope.home().to_path_buf(),
        workspace: inputs.cwd.to_path_buf(),
        scratch: vec![scratch.clone()],
    };
    let policy = Arc::new(Policy::from_set(inputs.capabilities, &paths));
    let process = processes(inputs, &scratch, &policy)
        .inspect_err(|err| tracing::warn!(error = %err, "terminal unavailable this session"))
        .ok();
    let browser = process.as_ref().and_then(|(sandbox, env)| {
        let shared = inputs.browser?;
        let exec = shared.exec(
            Arc::clone(sandbox),
            env.clone(),
            &scratch,
            inputs.image_input,
        );
        Some(Arc::new(exec))
    });
    let commands = process.and_then(|(sandbox, env)| {
        let config = scope.config();
        pmt_tools::terminal::doctor::local_shell(&config.terminal, inputs.launch, &sandbox)
            .inspect_err(|err| tracing::warn!(error = %err, "terminal unavailable this session"))
            .ok()
            .map(|shell| Commands {
                sandbox,
                env,
                shell,
            })
    });
    let terminal = commands
        .clone()
        .and_then(|commands| {
            terminal(inputs, &scratch, user_home.clone(), commands)
                .inspect_err(
                    |err| tracing::warn!(error = %err, "terminal unavailable this session"),
                )
                .ok()
        })
        .map(Arc::new);
    let env = SessionEnv {
        files: Some(Arc::new(files(inputs, places, user_home, policy))),
        terminal: terminal.clone(),
        web: web(scope).map(Arc::new),
        vision: None,
        browser: browser.clone(),
    };
    Ok(Attached {
        attachment: Environments::process().attach(inputs.session_id, env),
        browser,
        terminal,
        commands,
    })
}

fn files(
    inputs: &EnvInputs<'_>,
    places: Places,
    user_home: Option<PathBuf>,
    policy: Arc<Policy>,
) -> FileSession {
    let cwd = inputs.cwd.to_path_buf();
    let backend = LocalBackend::new(inputs.capabilities.clone(), places).denying_stores(policy);
    FileSession::new(FileSetup {
        backend: Arc::new(backend),
        cwd: Arc::new(move || cwd.clone()),
        user_home,
        control: ControlPaths {
            prometheus_home: inputs.scope.home().to_path_buf(),
        },
        read_max_chars: limit(inputs.config, |c| c.files.read_max_chars),
        linter: Linter::builtin(),
    })
}

/// The session's OS sandbox and the environment of the processes it
/// starts (terminal commands, `browser_exec`'s interpreter).
fn processes(
    inputs: &EnvInputs<'_>,
    scratch: &Path,
    policy: &Policy,
) -> Result<(Arc<Sandbox>, ChildEnv), Error> {
    let config = inputs.scope.config();
    let sandbox = Arc::new(Sandbox::new(policy.clone(), &config.security.sandbox)?);
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
    Ok((sandbox, env))
}

fn terminal(
    inputs: &EnvInputs<'_>,
    scratch: &Path,
    user_home: Option<PathBuf>,
    commands: Commands,
) -> Result<LocalTerminal, Error> {
    let config = inputs.scope.config();
    LocalTerminal::new(LocalSetup {
        terminal: config.terminal.clone(),
        max_chars: limit(inputs.config, |c| c.tool_output.max_chars),
        kill_grace: Duration::from_secs(2),
        sandbox: commands.sandbox,
        env: commands.env,
        shell: commands.shell,
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
        proxy: config.network.proxy.clone(),
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

/// A `live` size limit of the session's configuration, read at each use.
fn limit(config: &SharedScope, key: fn(&pmt_config::Config) -> u64) -> pmt_tools::setting::Limit {
    let config = config.clone();
    Arc::new(move || usize::try_from(key(config.get().config())).unwrap_or(usize::MAX))
}

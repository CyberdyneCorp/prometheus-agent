//! The session's local execution environment (spec: execution-
//! environments, tool-system): the file tools' backend, the terminal on
//! the local sandbox, the web tools and `browser_exec`, attached to the
//! process-wide table the built-in tools look their session up in.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_plugins::providers::Category;
use std::time::Duration;

use pmt_config::{ProfileScope, SharedScope};
use pmt_core::capability::{CapabilitySet, Places};
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::Redactor;
use pmt_net::OutboundPolicy;
use pmt_net::egress::Proxy;
use pmt_sandbox::{Paths, Policy, Sandbox};
use pmt_tools::approval::explain::Previewer;
use pmt_tools::browser::BrowserExec;
use pmt_tools::environments::{Attachment, Environments, SessionEnv};
use pmt_tools::files::{ControlPaths, CwdSource, FileSession, FileSetup, Linter, LocalBackend};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use pmt_tools::terminal::guard::GuardChain;
use pmt_tools::terminal::process::Notice;
use pmt_tools::terminal::shell::Shell;
use pmt_tools::terminal::{Terminal, TerminalSetup, spill};
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
    /// Where finished `notify_on_complete` background processes go.
    pub completions: Option<tokio::sync::mpsc::UnboundedSender<Notice>>,
    /// Provider objects of the profile's plugins (web backends by name).
    pub providers: pmt_plugins::providers::ProviderRegistry,
    /// What the session's plugin calls report through `session.info`.
    pub plugin_call: pmt_plugins::host::CallInfo,
    /// The profile's plugins (their `transform_terminal_output` hooks).
    pub plugins: &'a crate::plugins_host::ProfilePlugins,
    /// The session's egress policy.
    pub egress: &'a crate::egress::SessionEgress,
    /// Where its egress refusals are reported.
    pub denials: &'a Denials,
    /// Where `browser_vault` asks the person for a hidden value.
    pub vault_prompt: Option<Arc<dyn pmt_tools::browser::vault::MaskedPrompt>>,
    /// Receipts, quiet-wait and handoff of the background processes.
    pub processes: &'a super::processes::Wiring,
    /// Opens the file snapshots (undo and checkpoints), when on.
    pub snapshots: Option<super::snapshots::Opener<'a>>,
    /// Host directories whose files a command on a backend without the
    /// host filesystem may name (attachments, skills): uploaded on demand.
    pub input_roots: Vec<std::path::PathBuf>,
}

/// `browser_vault`'s masked prompt over a skill secret prompt (the CLI's
/// hidden line, or `secret.request` to a daemon session's clients).
pub struct VaultPrompt(pub Arc<dyn pmt_skills::secrets::SecretPrompt>);

impl pmt_tools::browser::vault::MaskedPrompt for VaultPrompt {
    fn ask<'a>(
        &'a self,
        name: &'a str,
        question: &'a str,
    ) -> pmt_tools::dispatch::SeamFuture<'a, Option<pmt_core::secret::SecretString>> {
        let prompt = Arc::clone(&self.0);
        let (name, question) = (name.to_owned(), question.to_owned());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || prompt.ask("browser_vault", &name, Some(&question)))
                .await
                .ok()
                .flatten()
        })
    }
}

/// Where a session's egress refusals go once its event sink exists. The
/// handle is weak: a refusal reported after the session closed is dropped,
/// and the trajectory writer stays solely owned for its finish.
pub type Denials =
    Arc<std::sync::OnceLock<std::sync::Weak<dyn pmt_agent::turn::events::EventSink>>>;

/// An attached environment.
pub struct Attached {
    /// Detaches the environment when dropped.
    pub attachment: Attachment,
    /// The session's `browser_exec` (its browsers close at session end).
    pub browser: Option<Arc<BrowserExec>>,
    /// The session's terminal (skills pass their declared secrets to it).
    pub terminal: Option<Arc<Terminal>>,
    /// Sync-back of a remote workspace (session end, `/sync`).
    pub sync: Option<Arc<pmt_tools::terminal::sync::SyncBack>>,
    /// File snapshots (`/undo`, `/rollback`, `/diff session`).
    pub snapshots: Option<Arc<pmt_tools::snapshots::SessionSnapshots>>,
    /// Where `/diff` runs git.
    pub worktree: pmt_tools::snapshots::worktree::Worktree,
    /// How the session runs a command: its sandbox, child environment and
    /// shell (skills' inline snippets run the same way).
    pub commands: Option<Commands>,
    /// The session's egress policy and its children's proxy.
    pub egress: crate::egress::Running,
    /// Where its egress refusals are reported.
    pub denials: Denials,
    /// Runs approval `explain` previews (read-only, offline).
    pub previewer: Option<Previewer>,
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
    let (places, paths) = places(inputs, user_home.as_deref(), &scratch);
    let policy = Arc::new(Policy::from_set(inputs.capabilities, &paths));
    let proxy = crate::egress::child_proxy(
        inputs.scope,
        inputs.capabilities,
        Arc::clone(&inputs.egress.hosts),
        "terminal",
        Arc::clone(&inputs.egress.observer),
    )?;
    let child_policy = match &proxy {
        Some(proxy) => crate::egress::through(policy.as_ref().clone(), proxy),
        None => policy.as_ref().clone(),
    };
    let process = processes(inputs, &scratch, &child_policy, proxy.as_ref())
        .inspect_err(|err| tracing::warn!(error = %err, "terminal unavailable this session"))
        .ok();
    let browser = process
        .as_ref()
        .and_then(|(sandbox, env)| browser_exec(inputs, sandbox, env, &scratch));
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
    let previewer = commands.as_ref().and_then(|c| previewer(inputs, &paths, c));
    let remote = remote(inputs, user_home.as_deref());
    let snapshots = inputs
        .snapshots
        .as_ref()
        .map(|opener| opener.open(inputs.cwd, remote.as_ref()));
    let worktree = super::snapshots::worktree(inputs.cwd, remote.as_ref());
    let terminal = commands.clone().and_then(|commands| {
        terminal(
            inputs,
            &scratch,
            user_home.clone(),
            commands,
            remote.as_ref(),
            snapshots.as_ref(),
        )
        .inspect_err(|err| tracing::warn!(error = %err, "terminal unavailable this session"))
        .ok()
        .map(Arc::new)
    });
    if let Some(terminal) = &terminal {
        inputs.processes.bind(terminal);
    }
    let (file_backend, sync) = remote
        .as_ref()
        .map(|r| remote_parts(inputs, r, terminal.as_ref()))
        .unwrap_or_default();
    let env = SessionEnv {
        files: Some(Arc::new(files(
            inputs,
            places,
            user_home,
            policy,
            file_backend,
            snapshots.as_ref(),
        ))),
        terminal: terminal.clone(),
        web: web(scope, &inputs.providers, &inputs.plugin_call, inputs.egress).map(Arc::new),
        vision: None,
        vault: browser.as_ref().map(|browser| vault(inputs, browser)),
        browser: browser.clone(),
    };
    Ok(Attached {
        attachment: Environments::process().attach(inputs.session_id, env),
        browser,
        terminal,
        sync,
        snapshots,
        worktree,
        commands,
        egress: crate::egress::Running::new(inputs.session_id, inputs.egress, proxy),
        denials: Arc::clone(inputs.denials),
        previewer,
    })
}

/// The session's non-local terminal backend (`None`: the local one).
fn remote(inputs: &EnvInputs<'_>, user_home: Option<&Path>) -> Option<super::backend::Remote> {
    let registry = super::backend::registry(inputs.plugins, &inputs.plugin_call);
    super::backend::select(
        inputs.scope,
        &registry,
        inputs.session_id,
        inputs.cwd,
        user_home,
    )
}

/// `browser_vault` of a session with a browser: the profile's vault (its
/// key in the OS keychain only where `vault.keyref` or
/// `vault.key_storage` says so).
fn vault(
    inputs: &EnvInputs<'_>,
    browser: &Arc<BrowserExec>,
) -> Arc<pmt_tools::browser::vault::VaultSession> {
    let keychain = Arc::new(pmt_config::secrets::OsKeychain);
    Arc::new(pmt_tools::browser::vault::VaultSession {
        vault: Arc::new(pmt_vault::Vault::of(inputs.scope.dir(), keychain)),
        storage: inputs.scope.config().vault.key_storage,
        browser: Arc::clone(browser),
        prompt: inputs.vault_prompt.clone(),
    })
}

/// The approval previewer: the session's shell (POSIX shells only: the
/// previews are `sh -c` lines) under a sandbox that reads the workspace,
/// scratch and toolchain and may neither write nor connect (spec:
/// tool-system, Approval explain and dry-run).
fn previewer(inputs: &EnvInputs<'_>, paths: &Paths, commands: &Commands) -> Option<Previewer> {
    if !commands.shell.kind.is_posix() {
        return None;
    }
    let policy = Policy::from_capabilities(PREVIEW_CAPABILITIES, paths).ok()?;
    let sandbox = Sandbox::new(policy, &inputs.scope.config().security.sandbox)
        .inspect_err(|err| tracing::warn!(error = %err, "approval previews unavailable"))
        .ok()?;
    Some(Previewer::new(
        Arc::new(sandbox),
        commands.env.clone(),
        commands.shell.path.clone(),
    ))
}

/// What an approval preview may do: read, and start the preview's own
/// programs; never write or connect.
const PREVIEW_CAPABILITIES: &[&str] = &[
    "fs.read:<workspace>/**",
    "fs.read:<scratch>/**",
    "fs.read:<toolchain>",
    "process.spawn:*",
];

/// The session's places (file tools) and sandbox paths.
fn places(inputs: &EnvInputs<'_>, user_home: Option<&Path>, scratch: &Path) -> (Places, Paths) {
    let home = user_home.map(Path::to_path_buf).unwrap_or_default();
    let places = Places {
        user_home: home.clone(),
        workspace: inputs.cwd.to_path_buf(),
        scratch: vec![scratch.to_path_buf()],
        toolchain: Vec::new(),
    };
    let paths = Paths {
        user_home: home,
        prometheus_home: inputs.scope.home().to_path_buf(),
        workspace: inputs.cwd.to_path_buf(),
        scratch: vec![scratch.to_path_buf()],
    };
    (places, paths)
}

/// Where a remote backend keeps the session's uploaded inputs.
const INPUTS_BASE: &str = "/tmp/.prometheus-inputs";

/// The file backend of a remote session and where its relative paths
/// start.
type RemoteFiles = (Arc<dyn pmt_tools::files::FileBackend>, CwdSource);

/// What a non-local backend adds: its file tools (relative paths follow the
/// terminal's cwd inside that environment), its sync-back, the upload of
/// host inputs commands name, and the container's idle reaper watching the
/// terminal.
fn remote_parts(
    inputs: &EnvInputs<'_>,
    remote: &super::backend::Remote,
    terminal: Option<&Arc<Terminal>>,
) -> (
    Option<RemoteFiles>,
    Option<Arc<pmt_tools::terminal::sync::SyncBack>>,
) {
    if let (Some(terminal), Some(container)) = (terminal, &remote.container) {
        let activity: Arc<dyn pmt_sandbox::container::Activity> = Arc::clone(terminal) as _;
        container.attach(Arc::downgrade(&activity));
    }
    if let Some(terminal) = terminal
        && !remote.target.descriptor().uses_host_filesystem
    {
        let base = format!("{INPUTS_BASE}/{}", inputs.session_id);
        terminal.set_inputs(Arc::new(pmt_tools::terminal::inputs::Inputs::new(
            &inputs.scope.config().terminal.backend,
            Arc::clone(&remote.exec),
            &base,
            &inputs.input_roots,
        )));
    }
    let cwd: CwdSource = if let Some(terminal) = terminal {
        let terminal = Arc::clone(terminal);
        Arc::new(move || terminal.cwd())
    } else {
        let start = remote.start_cwd.clone();
        Arc::new(move || start.clone())
    };
    let sync = remote.sync_back(&inputs.scope.config().terminal, inputs.cwd);
    (Some((Arc::clone(&remote.files), cwd)), sync)
}

fn files(
    inputs: &EnvInputs<'_>,
    places: Places,
    user_home: Option<PathBuf>,
    policy: Arc<Policy>,
    remote: Option<RemoteFiles>,
    snapshots: Option<&Arc<pmt_tools::snapshots::SessionSnapshots>>,
) -> FileSession {
    let host_cwd = inputs.cwd.to_path_buf();
    let (backend, cwd) = remote.unwrap_or_else(|| {
        let backend: Arc<dyn pmt_tools::files::FileBackend> =
            Arc::new(LocalBackend::new(inputs.capabilities.clone(), places).denying_stores(policy));
        (backend, Arc::new(move || host_cwd.clone()))
    });
    FileSession::new(FileSetup {
        backend,
        cwd,
        user_home,
        control: ControlPaths {
            prometheus_home: inputs.scope.home().to_path_buf(),
        },
        read_max_chars: limit(inputs.config, |c| c.files.read_max_chars),
        linter: Linter::builtin(),
        documents: super::documents::converter(inputs.scope, inputs.launch),
        edits: snapshots.map(|s| Arc::clone(s) as Arc<dyn pmt_tools::files::edits::EditRecorder>),
    })
}

/// The session's OS sandbox and the environment of the processes it
/// starts (terminal commands, `browser_exec`'s interpreter).
fn processes(
    inputs: &EnvInputs<'_>,
    scratch: &Path,
    policy: &Policy,
    proxy: Option<&Proxy>,
) -> Result<(Arc<Sandbox>, ChildEnv), Error> {
    let config = inputs.scope.config();
    let sandbox = Arc::new(Sandbox::new(policy.clone(), &config.security.sandbox)?);
    let audit = pmt_core::audit::AuditLog::of(inputs.scope.dir());
    for gap in sandbox.downgrades() {
        audit.record(
            &pmt_core::audit::Event::new(
                pmt_core::audit::Kind::Downgrade,
                gap.hardening.as_str(),
                "allowed",
            )
            .in_session(inputs.session_id)
            .with_detail(gap.reason.clone()),
        );
    }
    let mut env = ChildEnv::build(
        inputs.launch,
        EnvRequest {
            passthrough: &config.terminal.env_passthrough,
            ..EnvRequest::default()
        },
    );
    env.set_temp_dir(scratch);
    // `prometheus verify` run by the session records its evidence here.
    env.set("PROMETHEUS_SESSION_ID", inputs.session_id);
    if let Some(url) = proxy.map(pmt_net::egress::Proxy::url) {
        env.set_proxy(&url);
    }
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
    remote: Option<&super::backend::Remote>,
    snapshots: Option<&Arc<pmt_tools::snapshots::SessionSnapshots>>,
) -> Result<Terminal, Error> {
    let config = inputs.scope.config();
    let target: Arc<dyn pmt_tools::terminal::target::Target> = match remote {
        Some(remote) => Arc::clone(&remote.target),
        None => Arc::new(pmt_tools::terminal::target::LocalTarget::new(
            commands.sandbox,
            scratch,
        )),
    };
    Terminal::new(TerminalSetup {
        terminal: config.terminal.clone(),
        max_chars: limit(inputs.config, |c| c.tool_output.max_chars),
        kill_grace: Duration::from_secs(2),
        target,
        env: commands.env,
        shell: commands.shell,
        scratch: scratch.to_path_buf(),
        spill_dir: spill::session_dir(inputs.scope.dir(), inputs.session_id),
        start_cwd: remote.map_or_else(|| inputs.cwd.to_path_buf(), |r| r.start_cwd.clone()),
        user_home,
        redactor: inputs.redactor.clone(),
        hooks: vec![crate::plugins_host::terminal::output_hook(
            inputs.plugins.clone(),
            inputs.plugin_call.clone(),
        )],
        guards: GuardChain::default(),
        notices: remote.and_then(|r| r.notice.clone()).into_iter().collect(),
        notify: inputs.completions.clone(),
        auto_notify: inputs.processes.auto_notify,
        receipts: Some(Arc::clone(&inputs.processes.receipts)),
        before_command: snapshots
            .cloned()
            .map(|s| -> pmt_tools::terminal::BeforeCommand {
                Arc::new(move |command: &str, cwd: &Path| s.before_command(command, cwd))
            }),
        clock: None,
    })
    .map_err(|e| {
        Error::new(
            &pmt_core::error::codes::HOME_IO,
            format!("cannot set up the terminal: {e}"),
        )
    })
}

/// The web tools, when the profile can build an outbound client; a
/// backend id this build does not implement may name a plugin's provider.
fn web(
    scope: &ProfileScope,
    providers: &pmt_plugins::providers::ProviderRegistry,
    call: &pmt_plugins::host::CallInfo,
    egress: &crate::egress::SessionEgress,
) -> Option<WebSession> {
    let config = scope.config();
    let policy = OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
        proxy: config.network.proxy.clone(),
    };
    let client = pmt_net::client(&policy).ok()?;
    let destinations = crate::egress::destinations(&config.security.egress).ok()?;
    let fetcher = Arc::new(
        pmt_net::fetch::Fetcher::new(&policy, destinations)
            .ok()?
            .with_hosts(
                Arc::clone(&egress.hosts),
                "web",
                Some(Arc::clone(&egress.observer)),
            ),
    );
    let secrets = scope.secrets().clone();
    let lookup: pmt_tools::web::SecretLookup =
        Arc::new(move |name: &str| secrets.resolve(name).ok());
    let plugins = |capability, id: &str| {
        let category = match capability {
            pmt_tools::web::Capability::Search => Category::WebSearch,
            pmt_tools::web::Capability::Extract => Category::WebExtract,
        };
        providers.web_backend(category, id, call).ok()
    };
    Some(WebSession::from_config_with(
        &config.web,
        &client,
        fetcher,
        &lookup,
        FullTextStore::for_profile(scope.dir()),
        &plugins,
    ))
}

/// The session's `browser_exec`, when the profile has browser tools.
fn browser_exec(
    inputs: &EnvInputs<'_>,
    sandbox: &Arc<Sandbox>,
    env: &ChildEnv,
    scratch: &Path,
) -> Option<Arc<BrowserExec>> {
    let exec = inputs.browser?.exec(
        Arc::clone(sandbox),
        env.clone(),
        scratch,
        inputs.image_input,
        inputs.capabilities,
    );
    Some(Arc::new(exec))
}

/// A `live` size limit of the session's configuration, read at each use.
fn limit(config: &SharedScope, key: fn(&pmt_config::Config) -> u64) -> pmt_tools::setting::Limit {
    let config = config.clone();
    Arc::new(move || usize::try_from(key(config.get().config())).unwrap_or(usize::MAX))
}

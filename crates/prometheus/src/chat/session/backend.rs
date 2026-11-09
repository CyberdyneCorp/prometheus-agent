//! Which terminal backend a session runs on (spec: execution-environments,
//! Typed backend registry and selection; Per-profile terminal backends in
//! a shared gateway; Backend isolation descriptor; Working directory
//! resolution).
//!
//! Everything is read from the session's own `ProfileScope`, so two profiles
//! served by one gateway each get their own backend. `local` keeps the OS
//! sandbox target; `container`, `ssh` and `singularity` get a
//! [`RemoteTarget`] and file
//! tools that act inside that environment; a plugin backend gets a
//! [`PluginTarget`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_sandbox::backend::{BackendRegistry, Descriptor, Kind};
use pmt_sandbox::container::{ContainerSpec, Pool, Runtime, options};
use pmt_sandbox::ssh::SshTarget;
use pmt_tools::files::{FileBackend, RemoteFiles};
use pmt_tools::terminal::cwd::{Place, StartCwd, resolve};
use pmt_tools::terminal::remote::container::ContainerTransport;
use pmt_tools::terminal::remote::exec::PluginExec;
use pmt_tools::terminal::remote::singularity::SingularityTransport;
use pmt_tools::terminal::remote::ssh::SshTransport;
use pmt_tools::terminal::remote::{RemoteExec, RemoteTarget, Transport};
use pmt_tools::terminal::target::{PluginTarget, Target, Unavailable};

/// The release's sandbox image (`terminal.container.image` unset). The
/// release pipeline publishes it per version (spec: distribution-and-update,
/// Container image).
pub const SANDBOX_IMAGE: &str = concat!(
    "ghcr.io/cyberdynecorp/prometheus-sandbox:",
    env!("CARGO_PKG_VERSION")
);

/// A session's non-local backend.
pub struct Remote {
    /// Where commands run.
    pub target: Arc<dyn Target>,
    /// The file tools' backend inside it.
    pub files: Arc<dyn FileBackend>,
    /// Where the session starts.
    pub start_cwd: PathBuf,
    /// A one-time notice (a host path configured as `terminal.cwd`).
    pub notice: Option<String>,
    /// The container, whose idle reaper the terminal reports to.
    pub container: Option<Arc<ContainerTransport>>,
    /// Commands and files of the remote (sync-back).
    pub exec: Arc<dyn RemoteExec>,
    /// The remote workspace sync-back copies from, and the marker file of
    /// its window (`None`: nothing to sync, e.g. a mounted workspace).
    pub sync: Option<(String, String)>,
}

impl Remote {
    /// The session's sync-back, when `terminal.sync_back` is on and the
    /// backend has a workspace of its own.
    #[must_use]
    pub fn sync_back(
        &self,
        terminal: &pmt_config::schema::TerminalConfig,
        host_workspace: &Path,
    ) -> Option<Arc<pmt_tools::terminal::sync::SyncBack>> {
        let (root, marker) = self
            .sync
            .as_ref()
            .filter(|_| terminal.sync_back_enabled())?;
        let sync = Arc::new(pmt_tools::terminal::sync::SyncBack::new(
            Arc::clone(&self.exec),
            root,
            host_workspace,
            marker,
            terminal.sync_back_max_bytes,
        ));
        let opener = Arc::clone(&sync);
        // Opening the window connects; the session does not wait for it.
        std::thread::spawn(move || {
            if let Err(why) = opener.start() {
                tracing::warn!("sync-back window not opened yet: {why}");
            }
        });
        Some(sync)
    }
}

/// What the session's profile selects: `None` for `local`. A backend that
/// cannot be set up yields a target refusing every call with the reason,
/// never the local host.
#[must_use]
pub fn select(
    scope: &ProfileScope,
    registry: &BackendRegistry,
    session_id: &str,
    workspace: &Path,
    user_home: Option<&Path>,
) -> Option<Remote> {
    match try_select(scope, registry, session_id, workspace, user_home) {
        Ok(remote) => remote,
        Err(err) => {
            tracing::warn!(error = %err, "terminal backend unavailable this session");
            let exec: Arc<dyn RemoteExec> = Arc::new(Refused(err.message().to_owned()));
            Some(Remote {
                target: Arc::new(Unavailable::new(err.message(), Descriptor::ssh())),
                files: Arc::new(RemoteFiles::new(
                    &scope.config().terminal.backend,
                    Arc::clone(&exec),
                )),
                start_cwd: PathBuf::from("/"),
                notice: None,
                container: None,
                exec,
                sync: None,
            })
        }
    }
}

/// Remote file calls of an unavailable backend.
#[derive(Debug)]
struct Refused(String);

impl RemoteExec for Refused {
    fn exec(
        &self,
        _script: &str,
        _args: &[&str],
        _stdin: Option<&[u8]>,
    ) -> Result<pmt_tools::terminal::remote::ExecResult, String> {
        Err(self.0.clone())
    }
}

fn try_select(
    scope: &ProfileScope,
    registry: &BackendRegistry,
    session_id: &str,
    workspace: &Path,
    user_home: Option<&Path>,
) -> Result<Option<Remote>, Error> {
    let config = scope.config();
    let terminal = &config.terminal;
    let backend = registry.select(&terminal.backend)?;
    let descriptor = backend.descriptor(terminal);
    let mut held = None;
    let (place, transport): (Place, Option<Arc<dyn Transport>>) = match &backend.kind {
        Kind::Local | Kind::Reserved(_) => return Ok(None),
        Kind::Container => {
            let transport = container(scope, session_id, workspace, user_home)?;
            held = Some(Arc::clone(&transport));
            let place = Place::Container {
                workspace_mounted: terminal.container.mount_workspace,
                default_cwd: descriptor.default_cwd.clone(),
            };
            (place, Some(transport as Arc<dyn Transport>))
        }
        Kind::Ssh => {
            let control = scope.path("run/ssh");
            pmt_core::private_fs::create_dir(&control).ok();
            let target = SshTarget::from_config(&terminal.ssh, Some(&control))?;
            (Place::Ssh, Some(Arc::new(SshTransport::new(target))))
        }
        Kind::Singularity => {
            let place = Place::Container {
                workspace_mounted: false,
                default_cwd: descriptor.default_cwd.clone(),
            };
            (place, Some(singularity(scope)?))
        }
        Kind::Plugin => (
            Place::Remote {
                default_cwd: descriptor.default_cwd.clone(),
            },
            None,
        ),
    };
    let start = resolve(&StartCwd {
        place,
        configured: terminal.cwd.as_deref(),
        launch_dir: workspace,
        gateway_workspace: None,
        user_home,
    });
    let name = backend.name.clone();
    let sync_root = match &backend.kind {
        Kind::Ssh => Some(terminal.ssh.workspace.clone()),
        // A mounted workspace already is the host's.
        Kind::Container if terminal.container.mount_workspace => None,
        _ => Some(descriptor.default_cwd.clone()),
    };
    let remote = match (transport, &backend.plugin) {
        (Some(transport), _) => {
            let ssh = backend.kind == Kind::Ssh;
            let state = if ssh {
                pmt_tools::terminal::remote::SSH_STATE
            } else {
                pmt_tools::terminal::remote::CONTAINER_STATE
            };
            let mut remote = over_transport(&transport, descriptor, session_id, &name, state);
            // The marker lives outside the workspace; `~` is the remote
            // login directory for SSH (relative paths start there).
            let marker = if ssh {
                format!(".cache/prometheus-terminal/{session_id}-sync-marker")
            } else {
                format!("{state}/{session_id}-sync-marker")
            };
            remote.sync = sync_root.map(|root| (root, marker));
            remote
        }
        (None, Some(plugin)) => {
            let exec: Arc<dyn RemoteExec> = Arc::new(PluginExec(Arc::clone(plugin)));
            Remote {
                target: Arc::new(PluginTarget::new(Arc::clone(plugin))),
                files: Arc::new(RemoteFiles::new(&name, Arc::clone(&exec))),
                start_cwd: PathBuf::new(),
                notice: None,
                container: None,
                sync: sync_root
                    .map(|root| (root, format!("/tmp/.prometheus-{session_id}-sync-marker"))),
                exec,
            }
        }
        (None, None) => return Ok(None),
    };
    Ok(Some(Remote {
        start_cwd: PathBuf::from(start.cwd),
        notice: start.notice,
        container: held,
        ..remote
    }))
}

fn over_transport(
    transport: &Arc<dyn Transport>,
    descriptor: Descriptor,
    session_id: &str,
    name: &str,
    state: &str,
) -> Remote {
    let exec: Arc<dyn RemoteExec> = Arc::new(Arc::clone(transport));
    Remote {
        target: Arc::new(RemoteTarget::new(
            Arc::clone(transport),
            descriptor,
            session_id,
            state,
        )),
        files: Arc::new(RemoteFiles::new(name, Arc::clone(&exec))),
        start_cwd: PathBuf::new(),
        notice: None,
        container: None,
        exec,
        sync: None,
    }
}

/// The session's container: one per session, or per `shared_key`.
fn container(
    scope: &ProfileScope,
    session_id: &str,
    workspace: &Path,
    user_home: Option<&Path>,
) -> Result<Arc<ContainerTransport>, Error> {
    let config = scope.config();
    let options = &config.terminal.container;
    let runtime = Runtime::detect(&options.runtime)?;
    runtime.probe(&config.security.sandbox)?;
    let volumes = options::volumes(
        &options.volumes,
        user_home.unwrap_or_else(|| Path::new("/")),
        scope.home(),
    )?;
    let spec = ContainerSpec {
        runtime,
        profile: scope.name().to_string(),
        session: session_id.to_owned(),
        image: if options.image.is_empty() {
            SANDBOX_IMAGE.to_owned()
        } else {
            options.image.clone()
        },
        config: options.clone(),
        allow_degraded: config.security.sandbox.allow_degraded.clone(),
        workspace: options.mount_workspace.then(|| workspace.to_path_buf()),
        volumes,
        user: options.run_as_host_user.then(host_user).flatten(),
        idle_lifetime: Duration::from_secs(config.terminal.idle_lifetime_secs),
    };
    Ok(Arc::new(ContainerTransport::new(
        Pool::process().container(spec),
    )))
}

/// The profile's singularity instance (shared by its sessions).
fn singularity(scope: &ProfileScope) -> Result<Arc<dyn Transport>, Error> {
    let image = scope.config().terminal.singularity.image.clone();
    if image.is_empty() {
        return Err(Error::new(
            &pmt_core::error::codes::TERMINAL_BACKEND_UNAVAILABLE,
            "singularity backend requires terminal.singularity.image (a SIF path or docker:// reference)",
        ));
    }
    let runtime = pmt_sandbox::singularity::Runtime::detect()?;
    let spec = pmt_sandbox::singularity::InstanceSpec {
        profile: scope.name().to_string(),
        image,
        overlay: pmt_sandbox::singularity::overlay_dir(scope.dir()),
    };
    let instance = pmt_sandbox::singularity::Instance::shared(runtime, spec);
    Ok(Arc::new(SingularityTransport::new(instance)))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // `None` on other platforms
fn host_user() -> Option<(u32, u32)> {
    Some((
        nix::unistd::getuid().as_raw(),
        nix::unistd::getgid().as_raw(),
    ))
}

#[cfg(not(unix))]
fn host_user() -> Option<(u32, u32)> {
    None
}

/// The profile's backends: the built-ins and its enabled plugins'.
#[must_use]
pub fn registry(
    plugins: &crate::plugins_host::ProfilePlugins,
    call: &pmt_plugins::host::CallInfo,
) -> BackendRegistry {
    pmt_plugins::terminal::registry(&plugins.current(), call).0
}

/// Removes the profile's orphaned containers in the background, once per
/// gateway start (spec: execution-environments, Container lifecycle and
/// orphan reaping); nothing happens unless the profile uses `container`.
pub fn reap_orphans_in_background(scope: &ProfileScope) {
    let config = scope.config();
    if config.terminal.backend != "container" {
        return;
    }
    let setting = config.terminal.container.runtime.clone();
    let profile = scope.name().to_string();
    let idle = Duration::from_secs(config.terminal.idle_lifetime_secs);
    std::thread::spawn(move || {
        let reaped = Runtime::detect(&setting)
            .and_then(|runtime| pmt_sandbox::container::reap_orphans(&runtime, &profile, idle));
        match reaped {
            Ok(removed) if !removed.is_empty() => {
                tracing::info!(profile = %profile, "removed {} orphaned containers", removed.len());
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(profile = %profile, "orphan reaping failed: {}", err.message());
            }
        }
    });
}

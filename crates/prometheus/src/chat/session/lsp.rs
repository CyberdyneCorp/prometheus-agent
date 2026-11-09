//! The session's language-server diagnostics (spec: lsp-diagnostics):
//! `[lsp]`, the trust the session starts with, the managed runtimes that
//! install and run servers, and the sandbox they run in (the session's
//! child environment and egress proxy).

use std::sync::Arc;

use pmt_tools::lsp::install::Installs;
use pmt_tools::lsp::launch::SandboxLauncher;
use pmt_tools::lsp::workspace::{TrustInputs, Workspaces};
use pmt_tools::lsp::{LspSetup, SessionDiagnostics, Toolchain};
use pmt_tools::terminal::env::ChildEnv;

use super::env::EnvInputs;

/// The session's diagnostics, or `None` when `lsp.enabled = false`, the
/// backend does not use the host filesystem or no child environment could
/// be built.
pub(super) fn diagnostics(
    inputs: &EnvInputs<'_>,
    remote: Option<&super::backend::Remote>,
    env: Option<&ChildEnv>,
    proxy: Option<&pmt_net::egress::Proxy>,
) -> Option<SessionDiagnostics> {
    let uses_host_filesystem = remote.is_none_or(|r| r.target.descriptor().uses_host_filesystem);
    let config = inputs.scope.config().lsp.clone();
    if !config.enabled || !uses_host_filesystem {
        tracing::debug!(
            enabled = config.enabled,
            uses_host_filesystem,
            "no language-server diagnostics"
        );
        return None;
    }
    let (Some(env), Ok(handle)) = (env, tokio::runtime::Handle::try_current()) else {
        tracing::debug!("no language-server diagnostics: no child environment or runtime");
        return None;
    };
    let env = env.clone();
    let user_home = inputs.launch.user_home().cloned();
    let path_var = inputs.launch.inherited("PATH").map(ToOwned::to_owned);
    let workspaces = Workspaces::new(&TrustInputs {
        start_dir: inputs.cwd,
        automatic: inputs.automatic_trust,
        user_home: user_home.as_deref(),
        exclude_roots: &config.exclude_roots,
        trusted_workspaces: &config.trusted_workspaces,
    });
    let toolchain = toolchain(inputs, path_var.as_deref());
    let launcher = SandboxLauncher {
        prometheus_home: inputs.scope.home().to_path_buf(),
        user_home: user_home.unwrap_or_default(),
        sandbox: inputs.scope.config().security.sandbox.clone(),
        env,
        egress: proxy.map(crate::egress::through_later),
    };
    SessionDiagnostics::new(
        LspSetup {
            installs: Installs::of_home(inputs.scope.home(), path_var),
            config,
            profile_dir: inputs.scope.dir().to_path_buf(),
            session_id: inputs.session_id.to_owned(),
            workspaces,
            launcher: Arc::new(launcher),
            toolchain,
            handle,
        },
        uses_host_filesystem,
    )
}

/// The managed Node and CPython (installed lazily as `[runtimes]` allows);
/// none when the runtime store cannot be opened.
fn toolchain(inputs: &EnvInputs<'_>, path_var: Option<&std::ffi::OsStr>) -> Toolchain {
    crate::runtimes_cmd::browser::for_home(
        inputs.scope.home(),
        &inputs.scope.config().network,
        None,
        path_var,
    )
    .inspect_err(|err| tracing::debug!("language servers without managed runtimes: {err}"))
    .map(|policies| Toolchain {
        allow_lazy_install: policies.node.allow_lazy_install,
        node: Some(policies.node),
        python: Some(policies.python),
    })
    .unwrap_or_default()
}

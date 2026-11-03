//! The managed store as the browser tools see it (spec:
//! distribution-and-update, Lazy runtime installation policy; Persistent
//! runtime opt-outs): `pmt_tools::browser::Runtimes` over
//! `pmt_update::runtimes::lazy::Lazy`, the [`RuntimePolicy`]s a session's
//! browser setup takes (Chromium and the CPython that runs `browser_exec`)
//! and the availability probe of the browser tools.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::daemon::DaemonConfig;
use pmt_config::schema::{BrowserConfig, NetworkConfig};
use pmt_core::error::Error;
use pmt_net::OutboundPolicy;
use pmt_tools::browser::RuntimePolicy;
use pmt_tools::browser::launch::{InstallFuture, Runtimes};
use pmt_tools::probe::{FnProbe, Probe};
use pmt_update::runtimes::Store;
use pmt_update::runtimes::install::{Installer, Progress, Sources};
use pmt_update::runtimes::lazy::{Confirm, Lazy};

/// The id of the browser tools' availability probe.
pub const PROBE_ID: &str = "browser.runtimes";

/// The store-backed runtimes.
#[derive(Debug)]
pub struct StoreRuntimes {
    lazy: Lazy,
}

impl StoreRuntimes {
    /// Runtimes from `installer`'s store under the `[runtimes]` policy;
    /// `confirm` asks before a lazy install (`None` installs without
    /// asking, as a non-interactive session does).
    #[must_use]
    pub fn new(installer: Installer, config: &DaemonConfig, confirm: Option<Box<Confirm>>) -> Self {
        Self {
            lazy: Lazy::new(installer, config.runtimes.allow_lazy_install, confirm),
        }
    }
}

impl Runtimes for StoreRuntimes {
    fn installed(&self, name: &str) -> Option<PathBuf> {
        self.lazy.installed(name)
    }

    fn install<'a>(&'a self, name: &'a str) -> InstallFuture<'a> {
        Box::pin(async move {
            self.lazy
                .executable(name)
                .await
                .map_err(|e| e.message().to_owned())
        })
    }

    fn excluded(&self, name: &str) -> bool {
        self.lazy.excluded(name)
    }
}

/// What the browser tools run: Chromium (`chromium`) and the interpreter
/// of `browser_exec` (`cpython`), both over one store.
#[derive(Clone)]
pub struct Policies {
    /// Runtime `chromium`, else a Chromium-family browser of the host.
    pub chromium: RuntimePolicy,
    /// Runtime `cpython`, else `python3`/`python` on `PATH`.
    pub python: RuntimePolicy,
}

impl Policies {
    /// The browser tools' availability: `Err` carries the
    /// `unavailable:<reason>` reason (`excluded`, `lazy installs
    /// disabled`). Chromium is only needed for a local browser.
    ///
    /// # Errors
    ///
    /// The reason.
    pub fn availability(&self, browser: &BrowserConfig) -> Result<(), &'static str> {
        if needs_local_browser(browser) {
            self.chromium.availability("chromium")?;
        }
        self.python.availability("cpython")
    }
}

/// Whether sessions launch a local Chromium (no `browser.cdp_url`, no
/// cloud provider).
#[must_use]
pub fn needs_local_browser(browser: &BrowserConfig) -> bool {
    browser.cdp_url.is_none() && browser.provider == "local"
}

/// The policies over `installer`'s store with the lazy-install switch from
/// `daemon.toml`; `confirm` answers `Install <name> <version> (<size>)?
/// [Y/n]` before a lazy install (an interactive session asks the person;
/// `None` installs without asking); host executables are searched on
/// `path_var`.
#[must_use]
pub fn policies(
    installer: Installer,
    config: &DaemonConfig,
    confirm: Option<Box<Confirm>>,
    path_var: Option<&OsStr>,
) -> Policies {
    let runtimes = Arc::new(StoreRuntimes::new(installer, config, confirm));
    over(runtimes, config.runtimes.allow_lazy_install, path_var)
}

fn over(
    runtimes: Arc<dyn Runtimes>,
    allow_lazy_install: bool,
    path_var: Option<&OsStr>,
) -> Policies {
    let mut pythons = Vec::new();
    if let Some(path_var) = path_var {
        for name in ["python3", "python"] {
            pythons.extend(pmt_tools::terminal::shell::find_all(name, path_var, None));
        }
    }
    Policies {
        chromium: RuntimePolicy {
            runtimes: Arc::clone(&runtimes),
            allow_lazy_install,
            host_candidates: pmt_tools::browser::launch::system_candidates(path_var),
        },
        python: RuntimePolicy {
            runtimes,
            allow_lazy_install,
            host_candidates: pythons,
        },
    }
}

/// The store as the availability probe sees it: what is installed and
/// what is excluded, with no lease, HTTP client or install.
struct StoreView {
    store: Store,
    exclude: Vec<String>,
}

impl Runtimes for StoreView {
    fn installed(&self, name: &str) -> Option<PathBuf> {
        self.store.installed(name).map(|installed| installed.exe())
    }

    fn install<'a>(&'a self, name: &'a str) -> InstallFuture<'a> {
        Box::pin(async move { Err(format!("the availability probe never installs {name}")) })
    }

    fn excluded(&self, name: &str) -> bool {
        self.exclude.iter().any(|n| n == name)
    }
}

/// The browser's Chromium policy, installing without asking, with this
/// process's `PATH`.
#[must_use]
pub fn policy(installer: Installer, config: &DaemonConfig) -> RuntimePolicy {
    policies(installer, config, None, std::env::var_os("PATH").as_deref()).chromium
}

/// The policies of the home at `home`: its `daemon.toml`, its store and
/// the profile's network settings; install progress goes to stderr.
///
/// # Errors
///
/// An invalid `daemon.toml` (78), or `net.*` when no HTTP client can be
/// built.
pub fn for_home(
    home: &Path,
    network: &NetworkConfig,
    confirm: Option<Box<Confirm>>,
    path_var: Option<&OsStr>,
) -> Result<Policies, Error> {
    let config = DaemonConfig::load(home)?;
    let progress: Progress = Arc::new(|line| eprintln!("{line}"));
    let installer = installer(Store::open(home), &config, &outbound(network), progress)?;
    Ok(policies(installer, &config, confirm, path_var))
}

/// An installer for `store` with the home's sources (mirror, fallback,
/// exclusions).
///
/// # Errors
///
/// `net.*` when the HTTP client cannot be built.
pub fn installer(
    store: Store,
    config: &DaemonConfig,
    policy: &OutboundPolicy,
    progress: Progress,
) -> Result<Installer, Error> {
    let sources = Sources {
        mirror: config.runtimes.mirrors.runtimes.clone(),
        fallback: config.update.mirror_fallback,
        exclude: config.runtimes.exclude.clone(),
    };
    Installer::new(store, policy, sources, progress)
}

/// The outbound policy of a profile's `[network]`.
#[must_use]
pub fn outbound(network: &NetworkConfig) -> OutboundPolicy {
    OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(PathBuf::from),
        proxy: network.proxy.clone(),
    }
}

/// The browser tools' availability probe for the home at `home`: reads
/// `daemon.toml` and the store when it runs, so `runtimes exclude` and
/// installs take effect at the next surface build.
#[must_use]
pub fn probe(home: PathBuf) -> Arc<dyn Probe> {
    Arc::new(FnProbe::new(PROBE_ID, move |context| {
        let config = DaemonConfig::load(&home).map_err(|e| e.message().to_owned())?;
        let path_var = std::env::var_os("PATH");
        availability(
            Store::open(&home),
            &config,
            &context.config.browser,
            path_var.as_deref(),
        )
        .map_err(str::to_owned)
    }))
}

/// The browser tools' availability over `store` under `config`'s
/// `[runtimes]` policy (what the probe reports): `Err` carries the
/// `unavailable:<reason>` reason.
///
/// # Errors
///
/// The reason.
pub fn availability(
    store: Store,
    config: &DaemonConfig,
    browser: &BrowserConfig,
    path_var: Option<&OsStr>,
) -> Result<(), &'static str> {
    let view = StoreView {
        store,
        exclude: config.runtimes.exclude.clone(),
    };
    over(Arc::new(view), config.runtimes.allow_lazy_install, path_var).availability(browser)
}

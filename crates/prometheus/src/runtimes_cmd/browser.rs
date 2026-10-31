//! The managed store as the browser tools see it (spec:
//! distribution-and-update, Lazy runtime installation policy; Persistent
//! runtime opt-outs): `pmt_tools::browser::Runtimes` over
//! `pmt_update::runtimes::lazy::Lazy`, plus the [`RuntimePolicy`] a
//! session's browser setup takes.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::daemon::DaemonConfig;
use pmt_tools::browser::RuntimePolicy;
use pmt_tools::browser::launch::{InstallFuture, Runtimes};
use pmt_update::runtimes::install::Installer;
use pmt_update::runtimes::lazy::{Confirm, Lazy};

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

/// The browser's runtime policy for this home: the managed store, the
/// lazy-install switch from `daemon.toml`, and the host's Chromium-family
/// browsers.
#[must_use]
pub fn policy(installer: Installer, config: &DaemonConfig, interactive: bool) -> RuntimePolicy {
    let confirm: Option<Box<Confirm>> = interactive.then(|| Box::new(ask) as Box<Confirm>);
    RuntimePolicy {
        runtimes: Arc::new(StoreRuntimes::new(installer, config, confirm)),
        allow_lazy_install: config.runtimes.allow_lazy_install,
        host_candidates: pmt_tools::browser::launch::system_candidates(
            std::env::var_os("PATH").as_deref(),
        ),
    }
}

/// `Install <name> <version> (<size>)? [Y/n]` on the terminal; Enter is yes.
fn ask(question: &str) -> bool {
    eprint!("{question} ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "" | "y" | "Y" | "yes" | "Yes")
}

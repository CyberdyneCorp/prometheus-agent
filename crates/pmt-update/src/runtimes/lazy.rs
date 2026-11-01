//! The lazy installation policy (spec: distribution-and-update, Lazy
//! runtime installation policy; Persistent runtime opt-outs): a tool that
//! needs a missing runtime gets it installed on first use when
//! `runtimes.allow_lazy_install` allows it and `runtimes.exclude` does not
//! name it, after a `Install <name> <version> (<size>)? [Y/n]` question on
//! an interactive terminal. The version the caller uses stays leased for
//! the lifetime of this value.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use pmt_core::error::{Error, codes};

use super::install::{Installer, excluded};
use super::lease::Lease;

/// Answers the install question (asynchronously, so an interactive
/// surface can ask it while the session's turn waits); `None` (no
/// terminal) installs without asking.
pub type Confirm = dyn Fn(String) -> ConfirmFuture + Send + Sync;

/// The answer of a [`Confirm`]: `true` installs.
pub type ConfirmFuture = std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>;

/// Why a runtime a tool needs is unavailable without an install (the
/// `unavailable:<reason>` of the tool's availability probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// `runtimes.exclude` names it.
    Excluded,
    /// Lazy installs are disabled.
    LazyInstallsDisabled,
}

impl Unavailable {
    /// The probe reason.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Excluded => "excluded",
            Self::LazyInstallsDisabled => "lazy installs disabled",
        }
    }
}

/// The policy over one store.
pub struct Lazy {
    installer: Installer,
    allow_lazy_install: bool,
    confirm: Option<Box<Confirm>>,
    /// One lease per (name, version directory) used.
    leases: Mutex<BTreeMap<(String, String), Lease>>,
}

impl std::fmt::Debug for Lazy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lazy")
            .field("installer", &self.installer)
            .field("allow_lazy_install", &self.allow_lazy_install)
            .finish_non_exhaustive()
    }
}

impl Lazy {
    /// The policy: `installer` carries the store, sources and exclusions;
    /// `confirm` asks the install question (`None`: install without
    /// asking, for non-interactive sessions).
    #[must_use]
    pub fn new(
        installer: Installer,
        allow_lazy_install: bool,
        confirm: Option<Box<Confirm>>,
    ) -> Self {
        Self {
            installer,
            allow_lazy_install,
            confirm,
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether `runtimes.exclude` names `name`.
    #[must_use]
    pub fn excluded(&self, name: &str) -> bool {
        self.installer.excluded(name)
    }

    /// The installed, leased executable of `name`, if installed.
    #[must_use]
    pub fn installed(&self, name: &str) -> Option<PathBuf> {
        let store = self.installer_store();
        let installed = store.installed(name)?;
        let key = (name.to_owned(), installed.dir_name());
        let mut leases = self.leases.lock().unwrap_or_else(PoisonError::into_inner);
        if let std::collections::btree_map::Entry::Vacant(slot) = leases.entry(key) {
            let lease = Lease::acquire(store.root(), name, &slot.key().1).ok()?;
            slot.insert(lease);
        }
        Some(installed.exe())
    }

    /// Whether `name` can be used without an explicit install.
    ///
    /// # Errors
    ///
    /// The reason it cannot.
    pub fn availability(&self, name: &str) -> Result<(), Unavailable> {
        if self.installer_store().installed(name).is_some() {
            return Ok(());
        }
        if self.excluded(name) {
            return Err(Unavailable::Excluded);
        }
        if !self.allow_lazy_install {
            return Err(Unavailable::LazyInstallsDisabled);
        }
        Ok(())
    }

    /// The executable of `name`, installing the pinned version first when
    /// the policy allows it.
    ///
    /// # Errors
    ///
    /// `runtime.excluded` (1) / `runtime.not_installed` (69) with the spec
    /// messages, `runtime.declined` (130), or the install's errors.
    pub async fn executable(&self, name: &str) -> Result<PathBuf, Error> {
        if let Some(exe) = self.installed(name) {
            return Ok(exe);
        }
        let store = self.installer_store();
        let (pin, artifact) = store.pinned(name)?;
        if self.excluded(name) {
            return Err(excluded(name));
        }
        if !self.allow_lazy_install {
            return Err(Error::new(
                &codes::RUNTIME_NOT_INSTALLED,
                format!(
                    "{name} is not installed and lazy installs are disabled: run 'prometheus runtimes install {name}'"
                ),
            ));
        }
        let question = format!(
            "Install {name} {} ({})? [Y/n]",
            pin.version,
            human_size(artifact.size)
        );
        if let Some(confirm) = &self.confirm
            && !confirm(question).await
        {
            return Err(Error::new(
                &codes::RUNTIME_DECLINED,
                format!("{name} was not installed"),
            ));
        }
        self.installer.install(name).await?;
        self.installed(name).ok_or_else(|| {
            Error::new(
                &codes::RUNTIME_IO,
                format!("{name} was installed but cannot be leased"),
            )
        })
    }

    fn installer_store(&self) -> &super::Store {
        self.installer.store()
    }
}

/// `12.3 MB`-style sizes for the install question.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / 1_000_000.0;
    format!("{mb:.1} MB")
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_are_megabytes() {
        assert_eq!(human_size(103_982_159), "104.0 MB");
    }
}

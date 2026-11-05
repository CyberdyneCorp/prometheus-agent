//! The OS sandbox of a local browser (spec: execution-environments, Local
//! browser isolation).
//!
//! The browser reads the system toolchain and its own installation, writes
//! only its throwaway profile directory (under `<profile>/cache/browser/`)
//! and its private temp directory, and connects only to the session's
//! egress proxy: on macOS Seatbelt allows loopback to the proxy's port; on
//! Linux the browser gets its own network namespace whose only way out is
//! the proxy (Landlock + seccomp confine the rest); on Windows the
//! browser runs in an `AppContainer` with no network capability whose
//! only way out is its relay to the proxy. DevTools uses a pipe, so the
//! browser listens on nothing. Credential stores stay denied.

use std::path::{Path, PathBuf};

use pmt_config::schema::SandboxConfig;
use pmt_core::capability::{Capability, CapabilitySet};

/// Where a local browser's sandbox is anchored.
#[derive(Debug, Clone)]
pub struct Confinement {
    /// The user's home (its credential stores stay denied).
    pub user_home: PathBuf,
    /// `PROMETHEUS_HOME`.
    pub prometheus_home: PathBuf,
    /// `security.sandbox`: the mode and the allowed downgrades.
    pub sandbox: SandboxConfig,
    /// The executable that starts confined children on Windows
    /// ([`pmt_sandbox::Options::launcher`]); `None`: this executable.
    pub launcher: Option<PathBuf>,
}

/// What Linux browsers read beyond the toolchain: their own process
/// information and the CPU and memory topology.
#[cfg(target_os = "linux")]
const LINUX_READS: &[&str] = &["fs.read:/proc/**", "fs.read:/sys/**"];
#[cfg(not(target_os = "linux"))]
const LINUX_READS: &[&str] = &[];

/// The sandbox of a browser run from `executable` with `profile` and
/// `temp` as its only writable directories and `proxy` as its only way
/// out.
///
/// # Errors
///
/// The policy or the platform sandbox cannot be built.
pub(super) fn sandbox(
    executable: &Path,
    profile: &Path,
    temp: &Path,
    proxy: &pmt_net::egress::Proxy,
    confinement: &Confinement,
) -> Result<pmt_sandbox::Sandbox, String> {
    let paths = pmt_sandbox::Paths {
        user_home: confinement.user_home.clone(),
        prometheus_home: confinement.prometheus_home.clone(),
        workspace: profile.to_path_buf(),
        scratch: vec![temp.to_path_buf()],
    };
    let error = |e: pmt_core::error::Error| e.message().to_owned();
    let mut capabilities = [
        "fs.read:<workspace>/**",
        "fs.write:<workspace>/**",
        "fs.read:<scratch>/**",
        "fs.write:<scratch>/**",
        "fs.read:<toolchain>",
        "process.spawn:*",
        "net.connect:*",
    ]
    .iter()
    .chain(LINUX_READS)
    .map(|c| Capability::parse(c))
    .collect::<Result<Vec<_>, _>>()
    .map_err(error)?;
    // A path with spaces (an `.app` bundle) has no capability-string form.
    let installation = installation(executable);
    if !readable_by_every_container(&installation) {
        capabilities.push(Capability::read_under(&installation));
    }
    let set: CapabilitySet = capabilities.into_iter().collect();
    let policy = pmt_sandbox::Policy::from_set(&set, &paths).for_browser();
    let attacher = proxy.attacher();
    let relay = pmt_sandbox::Relay::new(move |attached| {
        attacher.serve(attached.listener, attached.alive);
    });
    let policy = policy.through_proxy(proxy.addr().port(), relay);
    let options = pmt_sandbox::Options {
        launcher: confinement.launcher.clone(),
        report: None,
    };
    pmt_sandbox::Sandbox::with_options(policy, &confinement.sandbox, options).map_err(error)
}

/// Whether every `AppContainer` already reads and executes `dir`: the
/// Windows and Program Files trees carry an `ALL APPLICATION PACKAGES`
/// entry, and a grant there would need rights this user lacks (a browser
/// installed for all users lives there). Elsewhere nothing is.
fn readable_by_every_container(dir: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "SystemRoot",
    ]
    .iter()
    .filter_map(std::env::var_os)
    .any(|root| dunce::simplified(dir).starts_with(Path::new(&root)))
}

/// The directory a browser executable is installed in: its `.app`
/// bundle on macOS (frameworks and helpers live beside the executable's
/// directory), else the executable's directory, symlinks resolved.
fn installation(executable: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    if let Some(bundle) = resolved
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
    {
        return bundle.to_path_buf();
    }
    resolved
        .parent()
        .map_or_else(|| resolved.clone(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn program_files_needs_no_grant() {
        let root = std::env::var_os("ProgramFiles").unwrap();
        let chrome = Path::new(&root).join(r"Google\Chrome\Application");
        assert!(readable_by_every_container(&chrome));
        let verbatim = PathBuf::from(format!(r"\\?\{}", chrome.display()));
        assert!(readable_by_every_container(&verbatim));
        assert!(!readable_by_every_container(Path::new(
            r"C:\Users\me\chrome"
        )));
    }

    #[test]
    #[cfg(unix)]
    fn an_app_bundle_is_installed_as_a_whole() {
        assert_eq!(
            installation(Path::new(
                "/nonexistent/Brave Browser.app/Contents/MacOS/Brave Browser"
            )),
            Path::new("/nonexistent/Brave Browser.app")
        );
        assert_eq!(
            installation(Path::new("/nonexistent/chrome-linux/chrome")),
            Path::new("/nonexistent/chrome-linux")
        );
    }
}

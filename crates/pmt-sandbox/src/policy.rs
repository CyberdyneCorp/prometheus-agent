//! Capability-to-policy translation (spec: security-boundaries, OS sandbox
//! per platform; Default sandbox denies credential stores).
//!
//! A child's effective capability set is reduced to what an OS sandbox can
//! enforce: readable and writable path roots, denied credential stores, TCP
//! ports and whether the child may start further processes. Capabilities with
//! no OS-level meaning (`llm.call`, `tools.call:*`, `secrets.read:*`, `ui.*`,
//! `browser.real_profile`) are accepted and ignored here; their checks live
//! in the host.
//!
//! The translation never widens a capability: a glob the OS cannot express
//! (a wildcard anywhere but a trailing `/**`) is dropped, so the sandbox is
//! narrower than the grant, and is listed in [`Policy::unrepresentable`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pmt_core::capability::{BASELINE, Capability, CapabilitySet};
use pmt_core::error::Error;

use crate::locations;

/// The directories a session's capabilities are written against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The user's home directory (`~`).
    pub user_home: PathBuf,
    /// `PROMETHEUS_HOME`; denied to children except `scratch`.
    pub prometheus_home: PathBuf,
    /// The session workspace (`<workspace>`).
    pub workspace: PathBuf,
    /// The session's own scratch and spill directories (`<scratch>`).
    pub scratch: Vec<PathBuf>,
}

/// Which TCP ports a child may use for one direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ports {
    /// Every port.
    All,
    /// Only these ports (empty: none).
    Only(BTreeSet<u16>),
}

impl Ports {
    fn none() -> Self {
        Self::Only(BTreeSet::new())
    }

    fn add(&mut self, port: Option<u16>) {
        match (self, port) {
            (this, None) => *this = Self::All,
            (Self::Only(ports), Some(port)) => {
                ports.insert(port);
            }
            (Self::All, Some(_)) => {}
        }
    }

    /// Whether every port is allowed.
    #[must_use]
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }
}

/// What the OS sandbox enforces for one child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub(crate) read: Vec<PathBuf>,
    pub(crate) write: Vec<PathBuf>,
    pub(crate) deny: Vec<PathBuf>,
    pub(crate) connect: Ports,
    pub(crate) bind: Ports,
    pub(crate) host_patterns: bool,
    pub(crate) spawn: bool,
    pub(crate) unrepresentable: Vec<String>,
}

impl Policy {
    /// Translates `capabilities` (the child's effective set) against `paths`.
    ///
    /// # Errors
    ///
    /// `capability.unknown` (exit 78) for a string outside the vocabulary.
    pub fn from_capabilities<S: AsRef<str>>(
        capabilities: &[S],
        paths: &Paths,
    ) -> Result<Self, Error> {
        let set = CapabilitySet::parse(capabilities)?;
        Ok(Self::from_set(&set, paths))
    }

    /// Translates a resolved capability set (`pmt_core::capability`).
    #[must_use]
    pub fn from_set(capabilities: &CapabilitySet, paths: &Paths) -> Self {
        let mut policy = Self {
            read: Vec::new(),
            write: Vec::new(),
            deny: locations::credential_stores(paths),
            connect: Ports::none(),
            bind: Ports::none(),
            host_patterns: false,
            spawn: false,
            unrepresentable: Vec::new(),
        };
        for capability in capabilities {
            policy.apply(capability, paths);
        }
        policy.read.sort();
        policy.read.dedup();
        policy.write.sort();
        policy.write.dedup();
        policy
    }

    /// The policy of [`BASELINE`].
    ///
    /// # Errors
    ///
    /// Never in practice: the baseline only uses known capabilities.
    pub fn baseline(paths: &Paths) -> Result<Self, Error> {
        Self::from_capabilities(BASELINE, paths)
    }

    fn apply(&mut self, capability: &Capability, paths: &Paths) {
        match capability {
            Capability::FsRead(glob) => self.add_paths(glob, paths, false),
            Capability::FsWrite(glob) => self.add_paths(glob, paths, true),
            Capability::NetConnect { host, port } => {
                self.host_patterns |= host != "*";
                self.connect.add(*port);
            }
            Capability::NetListen { port } => self.bind.add(*port),
            Capability::ProcessSpawn(_) => self.spawn = true,
            Capability::SecretsRead(_)
            | Capability::LlmCall(_)
            | Capability::ToolsCall(_)
            | Capability::ToolsOverride(_)
            | Capability::Ui(_)
            | Capability::BrowserRealProfile => {}
        }
    }

    fn add_paths(&mut self, pattern: &str, paths: &Paths, write: bool) {
        let Some(roots) = roots(pattern, paths) else {
            self.unrepresentable.push(pattern.to_owned());
            return;
        };
        let target = if write {
            &mut self.write
        } else {
            &mut self.read
        };
        target.extend(roots);
    }

    /// Readable (and executable) roots, canonical where they exist.
    #[must_use]
    pub fn read_roots(&self) -> &[PathBuf] {
        &self.read
    }

    /// Writable (and readable) roots, canonical where they exist.
    #[must_use]
    pub fn write_roots(&self) -> &[PathBuf] {
        &self.write
    }

    /// Every readable root, writable ones included.
    pub(crate) fn all_roots(&self) -> impl Iterator<Item = &PathBuf> {
        self.read.iter().chain(&self.write)
    }

    /// Credential stores that some granted root would otherwise expose:
    /// each lies strictly below a granted root. A store that a grant names
    /// exactly (or a path inside it) stays reachable through that grant.
    #[must_use]
    pub fn denied(&self) -> Vec<&Path> {
        self.deny
            .iter()
            .filter(|denied| self.all_roots().any(|root| is_strictly_below(denied, root)))
            .map(PathBuf::as_path)
            .collect()
    }

    /// Allowed outbound TCP ports.
    #[must_use]
    pub fn connect(&self) -> &Ports {
        &self.connect
    }

    /// Allowed listening TCP ports (loopback).
    #[must_use]
    pub fn bind(&self) -> &Ports {
        &self.bind
    }

    /// Whether some `net.connect` grant names a host rather than `*`; the OS
    /// sandbox can only filter ports, so host filtering waits for the egress
    /// proxy and the `net` feature is `partial`.
    #[must_use]
    pub fn has_host_patterns(&self) -> bool {
        self.host_patterns
    }

    /// Whether the child may start further processes.
    #[must_use]
    pub fn may_spawn(&self) -> bool {
        self.spawn
    }

    /// Patterns dropped because the OS cannot express them.
    #[must_use]
    pub fn unrepresentable(&self) -> &[String] {
        &self.unrepresentable
    }
}

/// The OS roots a path glob stands for, or `None` when it has a wildcard
/// the sandbox cannot express.
fn roots(pattern: &str, paths: &Paths) -> Option<Vec<PathBuf>> {
    if pattern == "<toolchain>" {
        return Some(locations::toolchain_roots());
    }
    let bases: Vec<(&Path, &str)> = if let Some(rest) = pattern.strip_prefix("<workspace>") {
        vec![(paths.workspace.as_path(), rest)]
    } else if let Some(rest) = pattern.strip_prefix("<scratch>") {
        paths
            .scratch
            .iter()
            .map(|dir| (dir.as_path(), rest))
            .collect()
    } else if pattern == "~" || pattern.starts_with("~/") {
        vec![(paths.user_home.as_path(), &pattern[1..])]
    } else {
        vec![(Path::new(""), pattern)]
    };
    bases
        .into_iter()
        .map(|(base, rest)| root(base, rest))
        .collect()
}

fn root(base: &Path, rest: &str) -> Option<PathBuf> {
    let literal = rest
        .strip_suffix("/**")
        .or_else(|| rest.strip_suffix("\\**"))
        .unwrap_or(rest);
    if literal.contains(['*', '?', '[', '{']) {
        return None;
    }
    let relative = literal.trim_start_matches(['/', '\\']);
    let path = if base.as_os_str().is_empty() {
        PathBuf::from(literal)
    } else if relative.is_empty() {
        base.to_path_buf()
    } else {
        base.join(relative)
    };
    path.is_absolute().then(|| locations::canonical(&path))
}

/// `path` is inside `root` and not `root` itself.
pub(crate) fn is_strictly_below(path: &Path, root: &Path) -> bool {
    path != root && path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute, nonexistent directory on this OS.
    fn base() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\nonexistent-x"
        } else {
            "/nonexistent-x"
        })
    }

    fn paths(base: &Path) -> Paths {
        Paths {
            user_home: base.join("home"),
            prometheus_home: base.join("home/.prometheus"),
            workspace: base.join("home/work"),
            scratch: vec![base.join("home/.prometheus/scratch/s1")],
        }
    }

    #[test]
    fn unknown_capability_is_named() {
        let p = paths(&base());
        let err = Policy::from_capabilities(&["filesystem.write:/tmp"], &p).unwrap_err();
        assert_eq!(err.message(), "unknown capability 'filesystem.write:/tmp'");
        assert_eq!(err.exit_code(), 78);
        for bad in [
            "net.listen:anywhere",
            "fs.read:",
            "ui.",
            "net.connect:host:port",
            "fs.read:relative/**",
        ] {
            assert!(Policy::from_capabilities(&[bad], &p).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_level_capabilities_are_accepted() {
        let p = paths(&base());
        let caps = [
            "llm.call",
            "llm.call:main",
            "tools.call:terminal",
            "secrets.read:GITHUB_TOKEN",
            "ui.tui",
            "browser.real_profile",
        ];
        let policy = Policy::from_capabilities(&caps, &p).unwrap();
        assert!(policy.read_roots().is_empty() && !policy.may_spawn());
    }

    #[test]
    fn placeholders_expand_and_wildcards_never_widen() {
        let p = paths(&base());
        let caps = [
            "fs.write:<workspace>/**",
            "fs.read:~/**",
            "fs.read:/etc/*.conf",
        ];
        let policy = Policy::from_capabilities(&caps, &p).unwrap();
        assert_eq!(policy.write_roots(), [base().join("home/work")]);
        assert_eq!(policy.read_roots(), [base().join("home")]);
        assert_eq!(policy.unrepresentable(), ["/etc/*.conf"]);
    }

    #[test]
    fn credential_stores_are_denied_below_broad_grants_only() {
        let p = paths(&base());
        let broad = Policy::from_capabilities(&["fs.read:~/**"], &p).unwrap();
        let denied = broad.denied();
        assert!(denied.contains(&base().join("home/.ssh").as_path()));
        assert!(denied.contains(&base().join("home/.aws").as_path()));
        assert!(denied.contains(&base().join("home/.prometheus").as_path()));

        let exact = Policy::from_capabilities(&["fs.read:~/.aws/config"], &p).unwrap();
        assert!(
            exact.denied().is_empty(),
            "an exact grant inside a store is honoured"
        );

        let workspace = Policy::baseline(&p).unwrap();
        assert!(
            !workspace
                .denied()
                .contains(&base().join("home/.ssh").as_path())
        );
    }

    #[test]
    fn network_ports_and_hosts() {
        let p = paths(&base());
        let policy = Policy::from_capabilities(
            &[
                "net.connect:api.github.com:443",
                "net.connect:[::1]:8080",
                "net.listen:loopback:9000",
            ],
            &p,
        )
        .unwrap();
        assert_eq!(policy.connect(), &Ports::Only([443, 8080].into()));
        assert_eq!(policy.bind(), &Ports::Only([9000].into()));
        assert!(policy.has_host_patterns());

        let any = Policy::from_capabilities(&["net.connect:*", "net.listen:loopback"], &p).unwrap();
        assert!(any.connect().is_all() && any.bind().is_all() && !any.has_host_patterns());
        let none = Policy::from_capabilities::<&str>(&[], &p).unwrap();
        assert_eq!(none.connect(), &Ports::none());
    }
}

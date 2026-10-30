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

use pmt_core::error::{Error, codes};

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

/// The built-in profile baseline used when `security.capabilities` is unset:
/// read/write on the workspace and scratch, read on the system toolchain,
/// any program, outbound connections per the egress policy (any public
/// host until the M2 egress proxy narrows it) and model calls.
pub const BASELINE: &[&str] = &[
    "fs.read:<workspace>/**",
    "fs.write:<workspace>/**",
    "fs.read:<scratch>/**",
    "fs.write:<scratch>/**",
    "fs.read:<toolchain>",
    "process.spawn:*",
    "net.connect:*",
    "llm.call",
];

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
            policy.apply(capability.as_ref(), paths)?;
        }
        policy.read.sort();
        policy.read.dedup();
        policy.write.sort();
        policy.write.dedup();
        Ok(policy)
    }

    /// The policy of [`BASELINE`].
    ///
    /// # Errors
    ///
    /// Never in practice: the baseline only uses known capabilities.
    pub fn baseline(paths: &Paths) -> Result<Self, Error> {
        Self::from_capabilities(BASELINE, paths)
    }

    fn apply(&mut self, capability: &str, paths: &Paths) -> Result<(), Error> {
        let unknown = || {
            Error::new(
                &codes::CAPABILITY_UNKNOWN,
                format!("unknown capability '{capability}'"),
            )
        };
        let Some((name, target)) = capability.split_once(':') else {
            return match capability {
                "llm.call" | "browser.real_profile" => Ok(()),
                ui if ui.strip_prefix("ui.").is_some_and(|s| !s.is_empty()) => Ok(()),
                _ => Err(unknown()),
            };
        };
        if target.is_empty() {
            return Err(unknown());
        }
        match name {
            "fs.read" => self.add_paths(target, paths, false),
            "fs.write" => self.add_paths(target, paths, true),
            "net.connect" => {
                let (host, port) = split_port(target).ok_or_else(unknown)?;
                self.host_patterns |= host != "*";
                self.connect.add(port);
            }
            "net.listen" => match target.strip_prefix("loopback") {
                Some("") => self.bind.add(None),
                Some(port) => {
                    let port = port.strip_prefix(':').and_then(|p| p.parse().ok());
                    self.bind.add(Some(port.ok_or_else(unknown)?));
                }
                None => return Err(unknown()),
            },
            "process.spawn" => self.spawn = true,
            "secrets.read" | "llm.call" | "tools.call" | "tools.override" => {}
            _ => return Err(unknown()),
        }
        Ok(())
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

/// `host[:port]`, where a bracketed IPv6 literal may carry a port.
fn split_port(target: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return match after {
            "" => Some((host, None)),
            port => Some((host, Some(port.strip_prefix(':')?.parse().ok()?))),
        };
    }
    match target.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Some((host, Some(port.parse().ok()?))),
        _ => Some((target, None)),
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
        let p = paths(Path::new("/x"));
        let err = Policy::from_capabilities(&["filesystem.write:/tmp"], &p).unwrap_err();
        assert_eq!(err.message(), "unknown capability 'filesystem.write:/tmp'");
        assert_eq!(err.exit_code(), 78);
        for bad in [
            "net.listen:anywhere",
            "fs.read:",
            "ui.",
            "net.connect:host:port",
        ] {
            assert!(Policy::from_capabilities(&[bad], &p).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_level_capabilities_are_accepted() {
        let p = paths(Path::new("/x"));
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
        let p = paths(Path::new("/x"));
        let caps = [
            "fs.write:<workspace>/**",
            "fs.read:~/**",
            "fs.read:/etc/*.conf",
            "fs.read:relative/**",
        ];
        let policy = Policy::from_capabilities(&caps, &p).unwrap();
        assert_eq!(policy.write_roots(), [PathBuf::from("/x/home/work")]);
        assert_eq!(policy.read_roots(), [PathBuf::from("/x/home")]);
        assert_eq!(policy.unrepresentable(), ["/etc/*.conf", "relative/**"]);
    }

    #[test]
    fn credential_stores_are_denied_below_broad_grants_only() {
        let p = paths(Path::new("/x"));
        let broad = Policy::from_capabilities(&["fs.read:~/**"], &p).unwrap();
        let denied = broad.denied();
        assert!(denied.contains(&Path::new("/x/home/.ssh")));
        assert!(denied.contains(&Path::new("/x/home/.aws")));
        assert!(denied.contains(&Path::new("/x/home/.prometheus")));

        let exact = Policy::from_capabilities(&["fs.read:~/.aws/config"], &p).unwrap();
        assert!(
            exact.denied().is_empty(),
            "an exact grant inside a store is honoured"
        );

        let workspace = Policy::baseline(&p).unwrap();
        assert!(!workspace.denied().contains(&Path::new("/x/home/.ssh")));
    }

    #[test]
    fn network_ports_and_hosts() {
        let p = paths(Path::new("/x"));
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

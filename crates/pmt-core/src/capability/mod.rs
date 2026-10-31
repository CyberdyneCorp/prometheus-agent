//! The typed capability vocabulary (spec: security-boundaries, Typed
//! capability vocabulary; Capability resolution never broadens).
//!
//! Every permission is one [`Capability`] from a closed vocabulary; any
//! other string is a load-time error naming it. [`Capability::covers`] is
//! the one "grants at least" relation every resolution step uses, and
//! [`CapabilitySet`] builds the resolutions on it: a session is the
//! baseline plus explicit grants, while subagents, plugins and tools can
//! only narrow (subset or intersection), never widen.
//!
//! `covers` is deliberately conservative: when it cannot prove that one
//! capability includes another (two unrelated wildcard globs, say) it
//! answers `false`, so resolution errs towards a narrower set.

mod path;
mod set;

use std::fmt;

use crate::error::{Error, codes};

pub use path::{Access, Places, canonical};
pub use set::{CapabilitySet, not_granted};

/// One permission.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// `fs.read:<glob>`.
    FsRead(String),
    /// `fs.write:<glob>` (writable paths are also readable).
    FsWrite(String),
    /// `net.connect:<host-pattern>[:<port>]`.
    NetConnect {
        /// `*`, `*.<suffix>` or a host name or address.
        host: String,
        /// `None`: every port.
        port: Option<u16>,
    },
    /// `net.listen:loopback[:<port>]`.
    NetListen {
        /// `None`: every port.
        port: Option<u16>,
    },
    /// `process.spawn:<program|*>`.
    ProcessSpawn(String),
    /// `secrets.read:<NAME>`.
    SecretsRead(String),
    /// `llm.call[:<model-role>]`; `None` covers every role.
    LlmCall(Option<String>),
    /// `tools.call:<tool|*>`.
    ToolsCall(String),
    /// `tools.override:<tool|*>`.
    ToolsOverride(String),
    /// `ui.<surface>`.
    Ui(String),
    /// `browser.real_profile`.
    BrowserRealProfile,
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

/// The wildcard target of `net.connect`, `process.spawn` and `tools.*`.
const ANY: &str = "*";

impl Capability {
    /// Parses one capability string.
    ///
    /// # Errors
    ///
    /// `capability.unknown` (exit 78): `unknown capability '<s>'`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        parse(text).ok_or_else(|| {
            Error::new(
                &codes::CAPABILITY_UNKNOWN,
                format!("unknown capability '{text}'"),
            )
            .with_hint(
                "capabilities are fs.read:<glob>, fs.write:<glob>, net.connect:<host>[:<port>], \
                 net.listen:loopback[:<port>], process.spawn:<program|*>, secrets.read:<NAME>, \
                 llm.call[:<role>], tools.call:<tool>, tools.override:<tool>, ui.<surface>, \
                 browser.real_profile",
            )
        })
    }

    /// `fs.read` on `dir` and everything below it, for a directory known
    /// only at run time (a profile's cache, a session's spill directory).
    #[must_use]
    pub fn read_under(dir: &std::path::Path) -> Self {
        Self::FsRead(path::subtree_glob(dir))
    }

    /// Whether holding `self` grants at least `other`.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        use Capability as C;
        match (self, other) {
            (C::FsRead(a), C::FsRead(b)) | (C::FsWrite(a), C::FsWrite(b) | C::FsRead(b)) => {
                path::glob_covers(a, b)
            }
            (C::NetConnect { host: a, port: pa }, C::NetConnect { host: b, port: pb }) => {
                host_covers(a, b) && port_covers(*pa, *pb)
            }
            (C::NetListen { port: a }, C::NetListen { port: b }) => port_covers(*a, *b),
            (C::ProcessSpawn(a), C::ProcessSpawn(b))
            | (C::ToolsCall(a), C::ToolsCall(b))
            | (C::ToolsOverride(a), C::ToolsOverride(b)) => a == ANY || a == b,
            (C::LlmCall(a), C::LlmCall(b)) => a.is_none() || a == b,
            _ => self == other,
        }
    }

    /// The largest capability both `self` and `other` cover, when one of
    /// them covers the other (otherwise `None`: the meet is dropped, which
    /// can only narrow).
    #[must_use]
    pub fn meet(&self, other: &Self) -> Option<Self> {
        if self.covers(other) {
            Some(other.clone())
        } else if other.covers(self) {
            Some(self.clone())
        } else {
            None
        }
    }
}

impl std::str::FromStr for Capability {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        Self::parse(text)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let port = |port: &Option<u16>| port.map(|p| format!(":{p}")).unwrap_or_default();
        match self {
            Self::FsRead(glob) => write!(f, "fs.read:{glob}"),
            Self::FsWrite(glob) => write!(f, "fs.write:{glob}"),
            Self::NetConnect { host, port: p } if host.contains(':') && p.is_some() => {
                write!(f, "net.connect:[{host}]{}", port(p))
            }
            Self::NetConnect { host, port: p } => write!(f, "net.connect:{host}{}", port(p)),
            Self::NetListen { port: p } => write!(f, "net.listen:loopback{}", port(p)),
            Self::ProcessSpawn(program) => write!(f, "process.spawn:{program}"),
            Self::SecretsRead(name) => write!(f, "secrets.read:{name}"),
            Self::LlmCall(None) => f.write_str("llm.call"),
            Self::LlmCall(Some(role)) => write!(f, "llm.call:{role}"),
            Self::ToolsCall(tool) => write!(f, "tools.call:{tool}"),
            Self::ToolsOverride(tool) => write!(f, "tools.override:{tool}"),
            Self::Ui(surface) => write!(f, "ui.{surface}"),
            Self::BrowserRealProfile => f.write_str("browser.real_profile"),
        }
    }
}

impl serde::Serialize for Capability {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::parse(&text).map_err(|e| serde::de::Error::custom(e.message()))
    }
}

fn parse(text: &str) -> Option<Capability> {
    let Some((name, target)) = text.split_once(':') else {
        return match text {
            "llm.call" => Some(Capability::LlmCall(None)),
            "browser.real_profile" => Some(Capability::BrowserRealProfile),
            ui => ui
                .strip_prefix("ui.")
                .filter(|s| is_word(s))
                .map(|s| Capability::Ui(s.to_owned())),
        };
    };
    if target.is_empty() || target.chars().any(char::is_whitespace) {
        return None;
    }
    let word = |target: &str| is_word(target).then(|| target.to_owned());
    let word_or_any = |target: &str| (target == ANY).then(|| ANY.to_owned()).or(word(target));
    match name {
        "fs.read" => path::normalize_glob(target).map(Capability::FsRead),
        "fs.write" => path::normalize_glob(target).map(Capability::FsWrite),
        "net.connect" => {
            let (host, port) = split_port(target)?;
            is_host_pattern(host).then(|| Capability::NetConnect {
                host: host.to_ascii_lowercase(),
                port,
            })
        }
        "net.listen" => match target.strip_prefix("loopback")? {
            "" => Some(Capability::NetListen { port: None }),
            port => Some(Capability::NetListen {
                port: Some(port.strip_prefix(':')?.parse().ok()?),
            }),
        },
        "process.spawn" => (target == ANY || !target.contains('*'))
            .then(|| Capability::ProcessSpawn(target.to_owned())),
        "secrets.read" => {
            is_secret_name(target).then(|| Capability::SecretsRead(target.to_owned()))
        }
        "llm.call" => word(target).map(|role| Capability::LlmCall(Some(role))),
        "tools.call" => word_or_any(target).map(Capability::ToolsCall),
        "tools.override" => word_or_any(target).map(Capability::ToolsOverride),
        _ => None,
    }
}

/// A tool, role or surface name: letters, digits, `_`, `-` and `.`.
fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// `^[A-Z][A-Z0-9_]{0,127}$`, the secret-name grammar.
fn is_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `*`, `*.<suffix>`, a host name or an IP address (no other wildcard).
fn is_host_pattern(host: &str) -> bool {
    let name = host.strip_prefix("*.").unwrap_or(host);
    host == ANY
        || (!name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_')))
}

/// `host[:port]`, where a bracketed IPv6 literal may carry a port and a
/// bare IPv6 literal carries none.
#[must_use]
pub fn split_port(target: &str) -> Option<(&str, Option<u16>)> {
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

fn host_covers(a: &str, b: &str) -> bool {
    a == ANY
        || a == b
        || a.strip_prefix('*')
            .is_some_and(|suffix| b.ends_with(suffix))
}

fn port_covers(a: Option<u16>, b: Option<u16>) -> bool {
    a.is_none() || a == b
}

#[cfg(test)]
mod tests;

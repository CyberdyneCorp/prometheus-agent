//! Grant subjects: who a grant is for.

use std::fmt;
use std::str::FromStr;

use pmt_core::error::{Error, codes};

/// The holder of a grant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Subject {
    /// `plugin:<id>@<digest>`: bound to the plugin's content digest, so an
    /// updated plugin does not inherit grants (M2 diff-grants).
    Plugin {
        /// The plugin id.
        id: String,
        /// Its content digest (`sha256:<hex>`).
        digest: String,
    },
    /// `mcp:<server>`.
    Mcp(String),
    /// `skill:<name>`.
    Skill(String),
    /// `profile`: the profile's own sessions (added to the baseline).
    Profile,
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn is_digest(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))
}

impl FromStr for Subject {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let parsed = match text.split_once(':') {
            None if text == "profile" => Some(Self::Profile),
            Some(("plugin", rest)) => rest.split_once('@').and_then(|(id, digest)| {
                (is_name(id) && is_digest(digest)).then(|| Self::Plugin {
                    id: id.to_owned(),
                    digest: digest.to_owned(),
                })
            }),
            Some(("mcp", name)) if is_name(name) => Some(Self::Mcp(name.to_owned())),
            Some(("skill", name)) if is_name(name) => Some(Self::Skill(name.to_owned())),
            _ => None,
        };
        parsed.ok_or_else(|| {
            Error::new(
                &codes::GRANTS_INVALID_SUBJECT,
                format!("invalid grant subject '{text}'"),
            )
            .with_hint("use plugin:<id>@<digest>, mcp:<server>, skill:<name> or profile")
        })
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plugin { id, digest } => write!(f, "plugin:{id}@{digest}"),
            Self::Mcp(name) => write!(f, "mcp:{name}"),
            Self::Skill(name) => write!(f, "skill:{name}"),
            Self::Profile => f.write_str("profile"),
        }
    }
}

impl serde::Serialize for Subject {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::Subject;

    #[test]
    fn subjects_round_trip_and_bad_ones_are_refused() {
        for text in [
            "plugin:tracker@sha256:4f2a",
            "mcp:github",
            "skill:deploy",
            "profile",
        ] {
            assert_eq!(text.parse::<Subject>().unwrap().to_string(), text);
        }
        for bad in [
            "plugin:tracker",
            "mcp:",
            "user:me",
            "profile:x",
            "skill:a b",
        ] {
            let err = bad.parse::<Subject>().unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}");
        }
    }
}

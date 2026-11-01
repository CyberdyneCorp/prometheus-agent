//! The release identity grammar (spec: distribution-and-update, Release
//! identity grammar): stable `vMAJOR.MINOR.PATCH` and canary
//! `vMAJOR.MINOR.PATCH-canary.YYYYMMDDHHMMSS` (UTC), where a canary carries
//! the next planned stable's numbers, so it sorts after the previous stable
//! and before its own.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A release channel. The install receipt stores it, never a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Tagged `vX.Y.Z` releases.
    Stable,
    /// `vX.Y.Z-canary.<timestamp>` builds of the next stable.
    Canary,
}

impl Channel {
    /// `stable` or `canary`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Canary => "canary",
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Channel {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "stable" => Ok(Self::Stable),
            "canary" => Ok(Self::Canary),
            other => Err(Error::new(
                &codes::UPDATE_INVALID_ARGUMENT,
                format!("--channel must be stable or canary, not {other}"),
            )),
        }
    }
}

impl Serialize for Channel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Channel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A release version. Ordering is semantic-version ordering restricted to
/// the grammar: a canary is lower than the stable of the same numbers, and
/// canaries of the same numbers order by timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReleaseVersion {
    major: u64,
    minor: u64,
    patch: u64,
    /// `YYYYMMDDHHMMSS` of a canary.
    canary: Option<u64>,
}

impl ReleaseVersion {
    /// The channel this version belongs to.
    #[must_use]
    pub fn channel(&self) -> Channel {
        if self.canary.is_some() {
            Channel::Canary
        } else {
            Channel::Stable
        }
    }

    /// Parses a release tag; the leading `v` is optional so a binary's own
    /// Cargo version (`1.4.0`) parses too.
    ///
    /// # Errors
    ///
    /// `update.invalid_argument` (exit 2) naming the grammar.
    pub fn parse(text: &str) -> Result<Self, Error> {
        parse(text).ok_or_else(|| {
            Error::new(
                &codes::UPDATE_INVALID_ARGUMENT,
                format!(
                    "'{text}' is not a release version (expected vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-canary.YYYYMMDDHHMMSS)"
                ),
            )
        })
    }
}

fn parse(text: &str) -> Option<ReleaseVersion> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let mut numbers = core.split('.').map(number);
    let (major, minor, patch) = (numbers.next()??, numbers.next()??, numbers.next()??);
    if numbers.next().is_some() {
        return None;
    }
    let canary = match pre {
        None => None,
        Some(pre) => Some(canary_timestamp(pre.strip_prefix("canary.")?)?),
    };
    Some(ReleaseVersion {
        major,
        minor,
        patch,
        canary,
    })
}

/// `0|[1-9][0-9]*`.
fn number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let leading_zero = text.len() > 1 && text.starts_with('0');
    if digits && !leading_zero {
        text.parse().ok()
    } else {
        None
    }
}

/// A UTC `YYYYMMDDHHMMSS` that names a real instant.
fn canary_timestamp(text: &str) -> Option<u64> {
    if text.len() != 14 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    jiff::civil::DateTime::strptime("%Y%m%d%H%M%S", text).ok()?;
    text.parse().ok()
}

impl Ord for ReleaseVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.canary, other.canary) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(a), Some(b)) => a.cmp(&b),
            })
    }
}

impl PartialOrd for ReleaseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)?;
        match self.canary {
            Some(ts) => write!(f, "-canary.{ts:014}"),
            None => Ok(()),
        }
    }
}

impl FromStr for ReleaseVersion {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        Self::parse(s)
    }
}

impl Serialize for ReleaseVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReleaseVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(|e| serde::de::Error::custom(e.message()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, ReleaseVersion};

    fn v(text: &str) -> ReleaseVersion {
        ReleaseVersion::parse(text).unwrap()
    }

    #[test]
    fn canary_orders_between_the_previous_and_its_own_stable() {
        let canary = v("v1.5.0-canary.20261029064100");
        assert!(canary > v("v1.4.0"));
        assert!(canary < v("v1.5.0"));
        assert!(canary < v("v1.5.0-canary.20261029064101"));
        assert_eq!(canary.channel(), Channel::Canary);
        assert_eq!(v("v1.4.0").channel(), Channel::Stable);
    }

    #[test]
    fn display_round_trips_and_the_v_is_optional() {
        assert_eq!(v("1.4.0").to_string(), "v1.4.0");
        let canary = "v1.5.0-canary.20261029064100";
        assert_eq!(v(canary).to_string(), canary);
    }

    #[test]
    fn the_grammar_is_strict() {
        for bad in [
            "v1.4",
            "v1.4.0.1",
            "v01.4.0",
            "v1.4.0-rc.1",
            "v1.4.0-canary.2026102906410",
            "v1.4.0-canary.20261329064100",
            "v1.4.0-canary.2026102906410a",
            "vx.y.z",
            "",
        ] {
            let err = ReleaseVersion::parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad}");
        }
    }

    #[test]
    fn unknown_channel_names_the_value() {
        let err = "main".parse::<Channel>().unwrap_err();
        assert_eq!(
            err.message(),
            "--channel must be stable or canary, not main"
        );
        assert_eq!(err.exit_code(), 2);
    }
}

//! The WIT worlds a plugin can implement and their versioning rules (spec:
//! plugin-system, WIT world versioning and compatibility).
//!
//! Each world is its own WIT package (`wit/<world>/world.wit`,
//! `prometheus:<world>@<major>.<minor>.0`), so worlds version
//! independently. The host instantiates a component built against the
//! current or the immediately previous major version of each world and
//! refuses any other. Within a major version WIT changes are additions
//! only; hook payload fields are added as optional JSON fields, which the
//! host strips for plugins built against an older minor version.

use std::fmt;
use std::str::FromStr;

use pmt_core::error::{Error, codes};
use semver::Version;

/// One world the host knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct World {
    /// The world (and package) name.
    pub name: &'static str,
    /// The host's version of it.
    pub major: u64,
    /// The host's minor version.
    pub minor: u64,
    /// The previous major version still instantiated, if one was released.
    pub previous_major: Option<u64>,
}

/// Every world, in the spec's order, with no previous major yet. `hook`
/// is at 1.3 (`console`), `tool` and `provider` at 1.1; the others are at
/// 1.0.
pub const WORLDS: &[World] = &[
    World {
        minor: 1,
        ..world("tool")
    },
    World {
        minor: 3,
        ..world("hook")
    },
    World {
        minor: 1,
        ..world("provider")
    },
    world("platform-adapter"),
    world("memory-provider"),
    world("context-engine"),
    world("terminal-backend"),
    world("ui-panel"),
    world("delegation-router"),
];

const fn world(name: &'static str) -> World {
    World {
        name,
        major: 1,
        minor: 0,
        previous_major: None,
    }
}

/// A world named with the version a plugin was built against
/// (`tool@1.0` in `plugin.toml`, `prometheus:tool/plugin@1.0.0` in a
/// component's exports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldRef {
    /// The world name.
    pub name: String,
    /// The version (a missing minor or patch is 0).
    pub version: Version,
}

impl WorldRef {
    /// The world a component export names: `prometheus:<world>/plugin@<v>`.
    #[must_use]
    pub fn from_export(name: &str) -> Option<Self> {
        let rest = name.strip_prefix("prometheus:")?;
        let (world, rest) = rest.split_once('/')?;
        // Only the world's `plugin` interface names it; other exported
        // interfaces of the package (`hook`'s `extensions`) are additions.
        let version = rest.strip_prefix("plugin@")?;
        Some(Self {
            name: world.to_owned(),
            version: Version::parse(version).ok()?,
        })
    }
}

impl FromStr for WorldRef {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let invalid = || {
            Error::new(
                &codes::PLUGINS_MANIFEST,
                format!("invalid world '{text}' (expected <name>@<version>, like tool@1.0)"),
            )
        };
        let (name, version) = text.split_once('@').ok_or_else(invalid)?;
        let mut parts = version.split('.');
        let mut next = || parts.next().map(str::parse::<u64>).transpose();
        let (Ok(Some(major)), Ok(minor), Ok(patch)) = (next(), next(), next()) else {
            return Err(invalid());
        };
        if parts.next().is_some() || name.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            name: name.to_owned(),
            version: Version::new(major, minor.unwrap_or(0), patch.unwrap_or(0)),
        })
    }
}

impl fmt::Display for WorldRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// How the host supports a world version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// The host's current major version.
    Current,
    /// The immediately previous major version (`prometheus plugins show`
    /// notes it as `<world>@<major> (previous major)`).
    PreviousMajor,
}

/// Checks a world reference against the host's worlds.
///
/// # Errors
///
/// `plugins.manifest` (65): `unknown world '<name>'`, or `world
/// '<name>@<version>' is not supported (host supports <range>)`.
pub fn check(world: &WorldRef) -> Result<Support, Error> {
    check_in(WORLDS, world)
}

/// [`check`] against another table of worlds (tests of the policy).
///
/// # Errors
///
/// As [`check`].
pub fn check_in(table: &[World], world: &WorldRef) -> Result<Support, Error> {
    let Some(known) = table.iter().find(|w| w.name == world.name) else {
        return Err(Error::new(
            &codes::PLUGINS_MANIFEST,
            format!("unknown world '{}'", world.name),
        )
        .with_hint(format!(
            "the worlds are {}",
            table.iter().map(|w| w.name).collect::<Vec<_>>().join(", ")
        )));
    };
    let major = world.version.major;
    if major == known.major && world.version.minor <= known.minor {
        return Ok(Support::Current);
    }
    if Some(major) == known.previous_major {
        return Ok(Support::PreviousMajor);
    }
    let mut range = format!("{}@{}.x", known.name, known.major);
    if let Some(previous) = known.previous_major {
        range = format!("{}@{previous}.x or {range}", known.name);
    }
    Err(Error::new(
        &codes::PLUGINS_MANIFEST,
        format!("world '{world}' is not supported (host supports {range})"),
    ))
}

/// The host's minor version of `world` (hook payload stripping).
#[must_use]
pub fn host_minor(world: &str) -> Option<u64> {
    WORLDS.iter().find(|w| w.name == world).map(|w| w.minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_ref(text: &str) -> WorldRef {
        text.parse().unwrap()
    }

    #[test]
    fn previous_major_still_loads_and_others_are_refused() {
        // Spec scenario "Previous major still loads": host at tool@3.x.
        let table = [World {
            name: "tool",
            major: 3,
            minor: 1,
            previous_major: Some(2),
        }];
        assert_eq!(
            check_in(&table, &world_ref("tool@2.4")).unwrap(),
            Support::PreviousMajor
        );
        assert_eq!(
            check_in(&table, &world_ref("tool@3.1")).unwrap(),
            Support::Current
        );
        let err = check_in(&table, &world_ref("tool@1.0")).unwrap_err();
        assert_eq!(
            err.message(),
            "world 'tool@1.0.0' is not supported (host supports tool@2.x or tool@3.x)"
        );
        // A newer minor than the host's is a newer world, not a compatible one.
        assert!(check_in(&table, &world_ref("tool@3.2")).is_err());
    }

    #[test]
    fn every_spec_world_is_known_and_typos_are_refused() {
        for name in [
            "tool",
            "hook",
            "provider",
            "platform-adapter",
            "memory-provider",
            "context-engine",
            "terminal-backend",
            "ui-panel",
            "delegation-router",
        ] {
            assert_eq!(
                check(&world_ref(&format!("{name}@1"))).unwrap(),
                Support::Current
            );
        }
        let err = check(&world_ref("tools@1.0")).unwrap_err();
        assert_eq!(err.message(), "unknown world 'tools'");
        assert!("tool".parse::<WorldRef>().is_err());
        assert!("tool@x".parse::<WorldRef>().is_err());
    }

    #[test]
    fn export_names_carry_the_world_version() {
        let w = WorldRef::from_export("prometheus:hook/plugin@1.3.0").unwrap();
        assert_eq!(w.to_string(), "hook@1.3.0");
        assert!(WorldRef::from_export("wasi:cli/run@0.2.0").is_none());
        assert!(WorldRef::from_export("prometheus:hook/extensions@1.1.0").is_none());
    }
}

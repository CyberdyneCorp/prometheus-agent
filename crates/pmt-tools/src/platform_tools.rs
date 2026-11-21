//! Platform-contributed toolsets (spec: platform-adapters,
//! Platform-contributed toolsets).
//!
//! An adapter may contribute typed tools ([`Contributor`]). They are
//! registered only into the registries of profiles that configure (enable)
//! the adapter's platform, in toolsets the platform's bundle
//! (`platform-<name>`) includes, so they reach sessions whose origin is
//! that platform, or sessions where the user enabled the toolset; a CLI
//! session of a profile without the platform never sees them. The
//! registry is built once at session start, so they are part of the
//! frozen tool surface.
//!
//! Actions needing platform permissions the bot lacks are hidden from the
//! schema. The adapter detects its permissions at connect and stores them
//! here ([`store_permissions`]) for 24 hours; a failed detection (or none
//! on record) shows every action. Every call is checked again against the
//! adapter's action allowlist ([`check_action`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use pmt_config::schema::{GatewayConfig, PlatformConfig};
use serde::{Deserialize, Serialize};

use crate::descriptor::{Owner, RegisteredTool};
use crate::registry::ToolRegistry;
use crate::toolset::Toolset;

/// How long detected permissions are trusted.
pub const PERMISSIONS_TTL: Duration = Duration::from_hours(24);

/// One action of a contributed tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// Its name.
    pub name: &'static str,
    /// The platform permission it needs, if any.
    pub permission: Option<&'static str>,
}

/// What a contributor builds its tools from.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    /// The owning profile's directory.
    pub profile_dir: &'a Path,
    /// The `[gateway.platforms.<name>]` table.
    pub config: &'a PlatformConfig,
    /// The permissions the bot holds (`None`: unknown, every action shown).
    pub granted: Option<BTreeSet<String>>,
}

/// An adapter's tools.
pub trait Contributor: Send + Sync {
    /// The platform whose adapter contributes them.
    fn platform(&self) -> &str;

    /// The toolsets they belong to, each with a description; the
    /// platform's bundle includes them (see
    /// [`crate::toolset::platform_toolsets`]).
    fn toolsets(&self) -> Vec<(String, String)>;

    /// The tools for one profile.
    fn tools(&self, context: &Context<'_>) -> Vec<RegisteredTool>;
}

fn contributors() -> &'static RwLock<Vec<Arc<dyn Contributor>>> {
    static ALL: RwLock<Vec<Arc<dyn Contributor>>> = RwLock::new(Vec::new());
    &ALL
}

/// Adds `contributor` for this process (once per platform; a second one
/// for the same platform replaces the first).
pub fn contribute(contributor: Arc<dyn Contributor>) {
    let mut all = contributors()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    all.retain(|c| c.platform() != contributor.platform());
    all.push(contributor);
}

/// Registers the contributed tools of every platform `gateway` enables;
/// answers the toolsets added. A tool that does not register is logged
/// and left out.
pub fn register(
    registry: &mut ToolRegistry,
    profile_dir: &Path,
    gateway: &GatewayConfig,
) -> Vec<String> {
    let all: Vec<Arc<dyn Contributor>> = contributors()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let mut added = Vec::new();
    for contributor in all {
        let platform = contributor.platform().to_owned();
        let Some(config) = gateway.platforms.get(&platform).filter(|p| p.enabled) else {
            continue;
        };
        let context = Context {
            profile_dir,
            config,
            granted: permissions(profile_dir, &platform, jiff::Timestamp::now()),
        };
        let tools = contributor.tools(&context);
        let owner = Owner::Capability(format!("platform:{platform}"));
        let sets: Vec<Toolset> = contributor
            .toolsets()
            .into_iter()
            .filter(|(name, _)| registry.toolset(name).is_none())
            .map(|(name, description)| {
                let members: Vec<&str> = tools
                    .iter()
                    .filter(|t| t.descriptor.toolset == name)
                    .map(RegisteredTool::name)
                    .collect();
                Toolset::new(&name, &description, &members)
            })
            .collect();
        added.extend(sets.iter().map(|s| s.name.clone()));
        if let Err(e) = registry.register_toolsets(&owner, sets) {
            tracing::warn!(platform, "platform toolsets not registered: {e}");
            continue;
        }
        for tool in tools {
            let name = tool.name().to_owned();
            if let Err(e) = registry.register(tool, &pmt_core::capability::CapabilitySet::new()) {
                tracing::warn!(platform, tool = name, "platform tool not registered: {e}");
            }
        }
    }
    added
}

/// The actions of `actions` a schema shows: those on `allowlist` (every
/// one without an allowlist) whose permission the bot holds (every one
/// when `granted` is unknown).
#[must_use]
pub fn visible<'a>(
    actions: &'a [Action],
    allowlist: Option<&[String]>,
    granted: Option<&BTreeSet<String>>,
) -> Vec<&'a Action> {
    actions
        .iter()
        .filter(|a| allowlist.is_none_or(|list| list.iter().any(|n| n == a.name)))
        .filter(|a| match (a.permission, granted) {
            (Some(needed), Some(granted)) => granted.contains(needed),
            _ => true,
        })
        .collect()
}

/// The call-time check of `action` against `allowlist`.
///
/// # Errors
///
/// `action <name> disabled by config`.
pub fn check_action(action: &str, allowlist: Option<&[String]>) -> Result<(), String> {
    if allowlist.is_none_or(|list| list.iter().any(|n| n == action)) {
        Ok(())
    } else {
        Err(format!("action {action} disabled by config"))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Detected {
    /// Unix seconds.
    checked_at: i64,
    /// `None`: the detection failed.
    granted: Option<BTreeSet<String>>,
}

fn permissions_file(profile_dir: &Path, platform: &str) -> PathBuf {
    profile_dir
        .join("cache")
        .join("platform-permissions")
        .join(format!("{platform}.json"))
}

/// Records what the adapter of `platform` detected at connect (`None`: the
/// detection failed).
///
/// # Errors
///
/// The write failed.
pub fn store_permissions(
    profile_dir: &Path,
    platform: &str,
    granted: Option<&BTreeSet<String>>,
) -> std::io::Result<()> {
    let detected = Detected {
        checked_at: jiff::Timestamp::now().as_second(),
        granted: granted.cloned(),
    };
    let path = permissions_file(profile_dir, platform);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    pmt_core::private_fs::write_atomic(&path, &serde_json::to_vec(&detected).unwrap_or_default())
}

/// The permissions on record for `platform` at `now`: `None` when none
/// were detected, the detection failed, or the record is older than
/// [`PERMISSIONS_TTL`].
#[must_use]
pub fn permissions(
    profile_dir: &Path,
    platform: &str,
    now: jiff::Timestamp,
) -> Option<BTreeSet<String>> {
    let bytes = std::fs::read(permissions_file(profile_dir, platform)).ok()?;
    let detected: Detected = serde_json::from_slice(&bytes).ok()?;
    let age = now.as_second() - detected.checked_at;
    let ttl = i64::try_from(PERMISSIONS_TTL.as_secs()).unwrap_or(i64::MAX);
    (0..ttl)
        .contains(&age)
        .then_some(detected.granted)
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIONS: &[Action] = &[
        Action {
            name: "fetch_messages",
            permission: None,
        },
        Action {
            name: "member_info",
            permission: Some("server_members"),
        },
        Action {
            name: "add_role",
            permission: Some("manage_roles"),
        },
    ];

    fn names(actions: &[&Action]) -> Vec<&'static str> {
        actions.iter().map(|a| a.name).collect()
    }

    #[test]
    fn missing_permissions_hide_actions_and_unknown_shows_all() {
        let granted: BTreeSet<String> = ["manage_roles".to_owned()].into();
        assert_eq!(
            names(&visible(ACTIONS, None, Some(&granted))),
            ["fetch_messages", "add_role"]
        );
        assert_eq!(names(&visible(ACTIONS, None, None)).len(), 3);
        let allow = vec!["fetch_messages".to_owned()];
        assert_eq!(
            names(&visible(ACTIONS, Some(&allow), None)),
            ["fetch_messages"]
        );
    }

    #[test]
    fn calls_are_rechecked_against_the_allowlist() {
        let allow = vec!["fetch_messages".to_owned()];
        assert_eq!(
            check_action("add_role", Some(&allow)).unwrap_err(),
            "action add_role disabled by config"
        );
        assert!(check_action("fetch_messages", Some(&allow)).is_ok());
        assert!(check_action("add_role", None).is_ok());
    }

    #[test]
    fn detected_permissions_last_a_day_and_a_failure_shows_all() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let granted: BTreeSet<String> = ["manage_roles".to_owned()].into();
        store_permissions(dir.path(), "discord", Some(&granted)).unwrap();
        let now = jiff::Timestamp::now();
        assert_eq!(permissions(dir.path(), "discord", now), Some(granted));
        let later = now + jiff::SignedDuration::from_hours(25);
        assert_eq!(permissions(dir.path(), "discord", later), None);
        store_permissions(dir.path(), "discord", None).unwrap();
        assert_eq!(permissions(dir.path(), "discord", now), None);
    }
}

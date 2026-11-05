//! User-approved, digest-bound plugin grants (spec: plugin-system,
//! Capability grants are user-approved and digest-bound; security-boundaries,
//! Grants pinned to plugin identity).
//!
//! A plugin's grants are rows of the profile's `grants.toml` whose subject
//! is `plugin:<id>@<digest>`. The digest covers every file of the plugin,
//! so a changed plugin is a new subject with no grants: it does not run
//! until the user approves it again. A running instance holds
//! [`PluginGrants`], which re-reads the file when it changes, so a
//! revocation applies to the next host call.

use pmt_config::grants::{Grant, Grants, GrantsFile, LiveGrants, Subject};
use pmt_config::secrets::surface::SurfaceOnly;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::error::{Error, codes};

use crate::manifest::{Manifest, label};

/// A plugin's identity: its id and content digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    /// The plugin id.
    pub id: String,
    /// `sha256:<hex>` of its files.
    pub digest: String,
}

impl Identity {
    /// The grant subject `plugin:<id>@<digest>`.
    #[must_use]
    pub fn subject(&self) -> Subject {
        Subject::Plugin {
            id: self.id.clone(),
            digest: self.digest.clone(),
        }
    }
}

/// Where a plugin's approval stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// It asks for nothing, or the user approved (all or some of) what this
    /// exact digest asks for: it runs with `granted`.
    Approved {
        /// Requested and granted.
        granted: CapabilitySet,
        /// Requested and not granted.
        pending: Vec<Capability>,
    },
    /// The user approved another digest of this id, not this one: the
    /// plugin changed and its grants no longer apply.
    Changed,
    /// Never approved.
    Unapproved,
}

impl Approval {
    /// Resolves `identity`'s approval of `requested` from `grants`.
    #[must_use]
    pub fn of(grants: &Grants, identity: &Identity, requested: &CapabilitySet) -> Self {
        let subject = identity.subject();
        let held = grants.capabilities(&subject);
        if requested.is_empty() || grants.grants.iter().any(|g| g.subject == subject) {
            return Self::Approved {
                granted: requested.intersect(&held),
                pending: held.missing(requested),
            };
        }
        let other_digest = grants
            .grants
            .iter()
            .any(|g| matches!(&g.subject, Subject::Plugin { id, .. } if *id == identity.id));
        if other_digest {
            Self::Changed
        } else {
            Self::Unapproved
        }
    }

    /// Why the plugin cannot run, for `plugins list` (`None` when approved).
    #[must_use]
    pub fn problem(&self, id: &str) -> Option<String> {
        match self {
            Self::Approved { .. } => None,
            Self::Changed => Some(format!(
                "plugin '{id}' changed since its capabilities were approved; run `prometheus plugins grants approve {id}`"
            )),
            Self::Unapproved => Some(format!(
                "capabilities not approved; run `prometheus plugins enable {id}`"
            )),
        }
    }
}

/// Whether `manifest` may hold surface-only secret `name`: only the
/// adapter plugin of the platform that owns it (a first-party surface's
/// credentials are read by host code alone, and one plugin platform's
/// adapter never reads another's).
fn may_hold(manifest: &Manifest, only: &SurfaceOnly, name: &str) -> bool {
    if !only.contains(name) {
        return true;
    }
    let provided = manifest.adapter_platform();
    only.plugin_owner(name)
        .is_some_and(|owner| provided.as_deref() == Some(owner))
}

/// Refuses `capabilities` when one reads a surface-only secret `manifest`
/// may not hold (spec: security-boundaries, Gateway-owned secrets are
/// never delegable).
///
/// # Errors
///
/// `secret.surface_only`: `secret '<NAME>' is surface-only`.
pub fn check_surface_only(
    manifest: &Manifest,
    only: &SurfaceOnly,
    capabilities: &CapabilitySet,
) -> Result<(), Error> {
    for capability in capabilities {
        if let Capability::SecretsRead(name) = capability
            && !may_hold(manifest, only, name)
        {
            return Err(pmt_config::secrets::surface::refusal(name));
        }
    }
    Ok(())
}

/// `capabilities` without the surface-only secrets `manifest` may not
/// hold: what a plugin runs with even when an older approval named one.
#[must_use]
pub fn withhold_surface_only(
    manifest: &Manifest,
    only: &SurfaceOnly,
    capabilities: &CapabilitySet,
) -> CapabilitySet {
    capabilities
        .iter()
        .filter(|c| !matches!(c, Capability::SecretsRead(name) if !may_hold(manifest, only, name)))
        .cloned()
        .collect()
}

/// Records the approval of `capabilities` for `identity`.
///
/// # Errors
///
/// `grants.store` when the file cannot be written.
pub fn approve(
    file: &GrantsFile,
    identity: &Identity,
    capabilities: &CapabilitySet,
    surface: &str,
) -> Result<Vec<Grant>, Error> {
    file.add(&identity.subject(), capabilities, surface)
}

/// Revokes `id`'s grants (every digest), only those `capability` covers
/// when given; returns what was removed.
///
/// # Errors
///
/// `plugins.not_found` (66) when nothing matched; `grants.store`.
pub fn revoke(
    file: &GrantsFile,
    id: &str,
    capability: Option<&Capability>,
) -> Result<Vec<Grant>, Error> {
    let matching: Vec<Grant> = file
        .load()?
        .grants
        .into_iter()
        .filter(|g| matches!(&g.subject, Subject::Plugin { id: p, .. } if p == id))
        .filter(|g| capability.is_none_or(|c| c.covers(&g.capability)))
        .collect();
    if matching.is_empty() {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' holds no matching grant"),
        ));
    }
    for grant in &matching {
        file.revoke(grant.id)?;
    }
    Ok(matching)
}

/// The capabilities `--grant` names out of `requested`: `all`, or a comma
/// list of capabilities in vocabulary form (`secrets.read:X`) or the
/// manifest's words (`secrets X`).
///
/// # Errors
///
/// `cli.usage` naming a capability the plugin did not request.
pub fn select(requested: &CapabilitySet, list: &str) -> Result<CapabilitySet, Error> {
    if list.trim() == "all" {
        return Ok(requested.clone());
    }
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|wanted| {
            requested
                .iter()
                .find(|c| c.to_string() == wanted || label(c) == wanted)
                .cloned()
                .ok_or_else(|| {
                    Error::new(
                        &codes::CLI_USAGE,
                        format!("the plugin does not request the capability '{wanted}'"),
                    )
                })
        })
        .collect()
}

/// A running instance's live view of its grants.
#[derive(Debug)]
pub struct PluginGrants {
    subject: Subject,
    requested: CapabilitySet,
    live: LiveGrants,
}

impl PluginGrants {
    /// Grants of `identity` within `requested`, read from `file`.
    #[must_use]
    pub fn new(identity: &Identity, requested: CapabilitySet, file: GrantsFile) -> Self {
        Self {
            subject: identity.subject(),
            requested,
            live: LiveGrants::new(file),
        }
    }

    /// Whose grants these are (`plugin:<id>@<digest>`).
    #[must_use]
    pub fn subject(&self) -> &Subject {
        &self.subject
    }

    /// Granted and requested now (an unreadable file grants nothing).
    #[must_use]
    pub fn current(&self) -> CapabilitySet {
        match self.live.current() {
            Ok(grants) => self
                .requested
                .intersect(&grants.capabilities(&self.subject)),
            Err(err) => {
                tracing::warn!(error = %err, "plugin grants unreadable; denying");
                CapabilitySet::new()
            }
        }
    }

    /// Checks `capability`, answering `capability_denied: <label>`.
    ///
    /// # Errors
    ///
    /// `plugins.capability_denied`.
    pub fn require(&self, capability: &Capability) -> Result<(), Error> {
        if self.current().allows(capability) {
            Ok(())
        } else {
            Err(denied(&label(capability)))
        }
    }
}

/// `capability_denied: <what>`.
#[must_use]
pub fn denied(what: &str) -> Error {
    Error::new(
        &codes::PLUGINS_CAPABILITY_DENIED,
        format!("capability_denied: {what}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(texts: &[&str]) -> CapabilitySet {
        CapabilitySet::parse(texts).unwrap()
    }

    fn identity(digest: &str) -> Identity {
        Identity {
            id: "weather".to_owned(),
            digest: digest.to_owned(),
        }
    }

    #[test]
    fn a_changed_digest_invalidates_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        let file = GrantsFile::at(dir.path().join("grants.toml"));
        let requested = caps(&["net.connect:api.weather.example"]);
        let v1 = identity("sha256:aa");
        assert_eq!(
            Approval::of(&file.load().unwrap(), &v1, &requested),
            Approval::Unapproved
        );
        approve(&file, &v1, &requested, "cli").unwrap();
        let approved = Approval::of(&file.load().unwrap(), &v1, &requested);
        assert!(matches!(approved, Approval::Approved { ref pending, .. } if pending.is_empty()));

        let v2 = identity("sha256:bb");
        assert_eq!(
            Approval::of(&file.load().unwrap(), &v2, &requested),
            Approval::Changed
        );
        let running = PluginGrants::new(&v2, requested.clone(), file.clone());
        let err = running
            .require(&Capability::parse("net.connect:api.weather.example").unwrap())
            .unwrap_err();
        assert_eq!(
            err.message(),
            "capability_denied: network api.weather.example"
        );
    }

    #[test]
    fn a_revocation_reaches_the_next_check() {
        let dir = tempfile::tempdir().unwrap();
        let file = GrantsFile::at(dir.path().join("grants.toml"));
        let requested = caps(&["secrets.read:NOTION_API_KEY", "net.connect:api.notion.com"]);
        let me = identity("sha256:aa");
        approve(&file, &me, &requested, "cli").unwrap();
        let running = PluginGrants::new(&me, requested, file.clone());
        let secret = Capability::parse("secrets.read:NOTION_API_KEY").unwrap();
        running.require(&secret).unwrap();
        revoke(&file, "weather", Some(&secret)).unwrap();
        assert!(running.require(&secret).is_err());
        running
            .require(&Capability::parse("net.connect:api.notion.com").unwrap())
            .unwrap();
    }

    #[test]
    fn grant_selection_takes_both_spellings_and_refuses_extras() {
        let requested = caps(&["secrets.read:GITHUB_TOKEN", "net.connect:api.github.com"]);
        assert_eq!(select(&requested, "all").unwrap(), requested);
        let one = select(&requested, "secrets GITHUB_TOKEN").unwrap();
        assert_eq!(one.to_strings(), vec!["secrets.read:GITHUB_TOKEN"]);
        let one = select(&requested, "net.connect:api.github.com").unwrap();
        assert_eq!(one.to_strings(), vec!["net.connect:api.github.com"]);
        assert!(select(&requested, "secrets OTHER").is_err());
    }

    #[test]
    fn a_partial_approval_runs_with_what_was_granted() {
        let dir = tempfile::tempdir().unwrap();
        let file = GrantsFile::at(dir.path().join("grants.toml"));
        let requested = caps(&["secrets.read:A", "secrets.read:B"]);
        let me = identity("sha256:aa");
        approve(&file, &me, &caps(&["secrets.read:A"]), "cli").unwrap();
        let Approval::Approved { granted, pending } =
            Approval::of(&file.load().unwrap(), &me, &requested)
        else {
            panic!("approved");
        };
        assert_eq!(granted.to_strings(), vec!["secrets.read:A"]);
        assert_eq!(pending, vec![Capability::parse("secrets.read:B").unwrap()]);
    }

    /// Gateway-owned secrets are never delegable: one plugin platform's
    /// adapter cannot hold another plugin platform's token, only its own.
    #[test]
    fn an_adapter_plugin_holds_only_its_own_platform_token() {
        let mut config = pmt_config::schema::Config::default();
        for (platform, token) in [("acme", "ACME_TOKEN"), ("zulu", "ZULU_TOKEN")] {
            let table = pmt_config::schema::PlatformConfig {
                token_secret: Some(pmt_config::schema::SecretName(token.into())),
                ..Default::default()
            };
            config.gateway.platforms.insert(platform.into(), table);
        }
        let only = SurfaceOnly::of(&config);
        let adapter = |id: &str| {
            let text = format!(
                "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
                 description = \"Adapter\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
                 worlds = [\"platform-adapter@1.0\"]\n"
            );
            Manifest::parse(&text, &semver::Version::new(0, 1, 0)).unwrap()
        };
        let (acme, zulu) = (adapter("acme"), adapter("zulu"));
        assert!(check_surface_only(&acme, &only, &caps(&["secrets.read:ACME_TOKEN"])).is_ok());
        let refused =
            check_surface_only(&zulu, &only, &caps(&["secrets.read:ACME_TOKEN"])).unwrap_err();
        assert_eq!(refused.message(), "secret 'ACME_TOKEN' is surface-only");
        let kept = withhold_surface_only(
            &zulu,
            &only,
            &caps(&["secrets.read:ACME_TOKEN", "secrets.read:ZULU_TOKEN"]),
        );
        assert_eq!(kept, caps(&["secrets.read:ZULU_TOKEN"]));
        let builtin = check_surface_only(&acme, &only, &caps(&["secrets.read:TELEGRAM_BOT_TOKEN"]));
        assert!(
            builtin.is_err(),
            "a first-party surface's token stays host-only"
        );
    }
}

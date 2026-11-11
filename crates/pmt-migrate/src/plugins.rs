//! The `plugins` component (spec: hermes-migration, MCP servers and
//! plugins; python-bridge, Migration of Hermes plugins).
//!
//! Hermes user plugins are copied for the Python bridge, audited and left
//! disabled with no grants: `bridge-compatible` ones are listed with the
//! command that reviews their capabilities, the rest are `unsupported`
//! with the APIs that block them. Hermes built-in integrations that
//! Prometheus provides as signed catalog plugins (Home Assistant,
//! Camofox, the catalog adapters) are listed with the plugin they need.

use pmt_core::error::{Error, codes};
use serde_json::Value;

use crate::component::Component;
use crate::host::Registry;
use crate::ledger::{self, Decision};
use crate::plan::{Dest, Source, Step};
use crate::report::{Disposition, Entry};
use crate::writes;

/// Hermes platforms Prometheus serves through catalog adapter plugins
/// (spec: platform-adapters, Catalog adapter set).
const CATALOG_ADAPTERS: &[&str] = &[
    "whatsapp_cloud",
    "msgraph_webhook",
    "teams",
    "google_chat",
    "mattermost",
    "dingtalk",
    "feishu",
    "wecom",
    "wecom_callback",
    "weixin",
    "bluebubbles",
    "photon",
    "qqbot",
    "yuanbao",
    "line",
    "sms",
    "ntfy",
    "irc",
    "raft",
    "buzz",
    "simplex",
    "homeassistant",
];

/// The planned plugins component.
pub struct PluginsStep {
    entries: Vec<Entry>,
    dir: Option<std::path::PathBuf>,
    enabled: Vec<String>,
    /// Entry index per plugin id.
    planned: Vec<(String, usize)>,
}

impl Step for PluginsStep {
    fn component(&self) -> Component {
        Component::Plugins
    }

    fn entries(&mut self) -> &mut Vec<Entry> {
        &mut self.entries
    }

    fn apply(&mut self, dest: &Dest<'_>) -> Result<(), Error> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let dir = dir.as_path();
        let settled = self.prepare(dir, dest)?;
        let outcomes = dest.host.import_plugins(dest.scope, dir, &self.enabled)?;
        for outcome in outcomes {
            let Some((_, index)) = self.planned.iter().find(|(id, _)| *id == outcome.id) else {
                continue;
            };
            if settled.contains(index) {
                continue;
            }
            let e = &mut self.entries[*index];
            let installed = dest.scope.dir().join("plugins").join(&outcome.id);
            if outcome.skipped.is_none() {
                let hash = pmt_skills::digest::dir(&dir.join(&outcome.id));
                dest.ledger.borrow_mut().record(
                    format!("plugin:{}", outcome.id),
                    hash,
                    pmt_skills::digest::dir(&installed),
                );
            }
            if let Some(why) = outcome.skipped {
                e.disposition = Disposition::Conflict;
                e.reason = format!("{why}; kept");
            } else if outcome.bridge_compatible {
                e.disposition = Disposition::Mapped;
                e.reason = compatible_reason(&outcome.id, outcome.was_enabled, &outcome.proposed);
            } else {
                e.disposition = Disposition::Unsupported;
                e.reason = format!(
                    "unsupported on the Python bridge (blocked by {}); copied disabled",
                    outcome.blocking.join(", ")
                );
            }
        }
        Ok(())
    }
}

impl PluginsStep {
    /// Settles each plugin against the ledger before the import, which
    /// copies only plugins the profile does not have: one unchanged since
    /// the last run is left alone, and one changed in Hermes whose copy
    /// still holds what the previous run wrote is removed so it is
    /// copied again. Returns the entries settled here.
    fn prepare(&mut self, dir: &std::path::Path, dest: &Dest<'_>) -> Result<Vec<usize>, Error> {
        let mut settled = Vec::new();
        for (id, index) in &self.planned {
            let installed = dest.scope.dir().join("plugins").join(id);
            let hash = pmt_skills::digest::dir(&dir.join(id));
            let current = installed
                .is_dir()
                .then(|| pmt_skills::digest::dir(&installed));
            let prior = dest.ledger.borrow().get(&format!("plugin:{id}")).cloned();
            match ledger::decide(
                prior.as_ref(),
                &hash,
                &hash,
                current.as_deref(),
                dest.overwrite,
            ) {
                Decision::Unchanged => {
                    writes::unchanged(&mut self.entries[*index]);
                    settled.push(*index);
                }
                Decision::Write if current.is_some() => {
                    std::fs::remove_dir_all(&installed).map_err(|e| {
                        Error::io_doing(&codes::HOME_IO, "cannot replace", &installed, &e)
                    })?;
                }
                _ => {}
            }
        }
        Ok(settled)
    }
}

fn compatible_reason(id: &str, was_enabled: bool, proposed: &[String]) -> String {
    let mut reason = format!(
        "bridge-compatible; imported disabled with no grants; run `prometheus plugins enable {id}` to review its capabilities"
    );
    if !proposed.is_empty() {
        reason = format!("{reason} ({})", proposed.join(", "));
    }
    if was_enabled {
        reason.push_str("; it was enabled in Hermes");
    }
    reason
}

/// Plans the plugins component.
///
/// # Errors
///
/// `migrate.component` when the plugins directory cannot be listed.
pub fn plan(src: &Source<'_>) -> Result<PluginsStep, Error> {
    let enabled = names(src.get("plugins.enabled"));
    let mut step = PluginsStep {
        entries: Vec::new(),
        dir: None,
        enabled: enabled.clone(),
        planned: Vec::new(),
    };
    let dir = src.profile.path("plugins");
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().join("plugin.yaml").is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    if !ids.is_empty() {
        step.dir = Some(dir);
    }
    for id in ids {
        step.planned.push((id.clone(), step.entries.len()));
        step.entries.push(
            Entry::new(
                Component::Plugins,
                format!("plugins/{id}"),
                Disposition::Mapped,
            )
            .to(format!("plugins/{id}"))
            .because("copied for the Python bridge, disabled, no grants"),
        );
    }
    for key in ["plugins.enabled", "plugins.disabled"] {
        if src.get(key).is_some() {
            step.entries.push(
                Entry::new(Component::Plugins, key, Disposition::Transformed)
                    .because("every imported plugin arrives disabled; see each plugin"),
            );
        }
    }
    integrations(src, &mut step);
    Ok(step)
}

fn names(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    }
}

/// Hermes built-in integrations the profile used whose Prometheus
/// counterpart is a catalog plugin.
fn integrations(src: &Source<'_>, step: &mut PluginsStep) {
    let mut needed: Vec<(&str, &str)> = Vec::new();
    let platform_used = |p: &str| {
        src.get(&format!("platforms.{p}.enabled"))
            .and_then(Value::as_bool)
            == Some(true)
    };
    if src.env_value("HASS_TOKEN").is_some() || src.get("platforms.homeassistant").is_some() {
        needed.push(("homeassistant", "Home Assistant"));
    }
    let camofox = src.get("browser.cloud_provider").and_then(Value::as_str) == Some("camofox");
    if camofox || src.env_value("CAMOFOX_URL").is_some() {
        needed.push(("camofox", "the Camofox browser"));
    }
    for adapter in CATALOG_ADAPTERS {
        let via_env = src.env.iter().any(|e| {
            e.name
                .to_ascii_lowercase()
                .starts_with(&format!("{adapter}_"))
                && crate::env::secret_shaped(&e.name)
        });
        let builtin = src.host.known(Registry::Platform, adapter, None);
        if !builtin && *adapter != "homeassistant" && (platform_used(adapter) || via_env) {
            needed.push((adapter, "a messaging platform"));
        }
    }
    for (id, what) in needed {
        step.entries.push(
            Entry::new(Component::Plugins, format!("integration:{id}"), Disposition::Unsupported)
                .to(format!("catalog plugin {id}"))
                .because(format!(
                    "Hermes used {what}; Prometheus serves it with the signed catalog plugin '{id}', which this catalog does not offer yet (install it disabled with `prometheus plugins install {id}` once it does)"
                )),
        );
    }
}

//! A profile's automation blueprint catalog (spec: scheduled-jobs,
//! Automation blueprint catalog; Suggested automations inbox): the
//! built-ins, then each enabled plugin's (`[[automation_blueprints]]` in
//! its manifest, or a Python plugin's `register_automation_blueprint`
//! calls as its bridge last cached them), keys `<plugin>:<key>`, then each
//! installed skill's `blueprint` block, key `skill:<key>`. A blueprint that
//! does not check is skipped with a WARN; none replaces a built-in.

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_cron::blueprint::{Blueprint, Origin, builtin};
use pmt_cron::suggest::{self, Source, Suggestion};
use pmt_plugins::load::State;
use pmt_plugins::manifest::Kind;
use pmt_store::Store;
use serde_json::Value;

/// Adds `blueprint` unless its key is taken.
fn push(all: &mut Vec<Blueprint>, blueprint: Result<Blueprint, pmt_core::error::Error>) {
    match blueprint {
        Ok(b) if all.iter().any(|known| known.key == b.key) => {
            tracing::warn!(key = b.key, "blueprint skipped: its key is taken");
        }
        Ok(b) => all.push(b),
        Err(err) => tracing::warn!("blueprint skipped: {}", err.message()),
    }
}

/// The blueprints of the enabled plugins, as declared data.
fn plugin_blueprints(scope: &ProfileScope) -> Vec<(String, Value)> {
    let version = crate::plugins_host::running_version();
    let blocked = crate::plugins_host::catalog::kill_list(scope);
    let cwd = scope.dir().to_path_buf();
    let inputs = crate::plugins_host::inputs(scope, &cwd, &version, &blocked);
    let mut found = Vec::new();
    for (plugin, status) in pmt_plugins::load::survey(&inputs) {
        let Ok(manifest) = &plugin.manifest else {
            continue;
        };
        if status.state != State::Loaded {
            continue;
        }
        let declared: Vec<Value> = if manifest.kind == Kind::Python {
            pmt_pybridge::registrations::latest_cache(scope.dir(), &manifest.id)
                .map(|r| r.blueprints)
                .unwrap_or_default()
        } else {
            manifest
                .automation_blueprints
                .iter()
                .filter_map(|t| serde_json::to_value(t).ok())
                .collect()
        };
        found.extend(declared.into_iter().map(|v| (manifest.id.clone(), v)));
    }
    found
}

/// The skills' `blueprint` blocks, by skill name.
fn skill_blueprints(scope: &ProfileScope, launch: &LaunchEnv) -> Vec<(String, Value)> {
    let catalog = pmt_skills::eject::profile_catalog(
        scope,
        None,
        launch.user_home().map(std::path::PathBuf::as_path),
        pmt_skills::bundled::Bundled::builtin(),
    );
    let disabled = &scope.config().skills.disabled;
    catalog
        .skills
        .iter()
        .filter(|s| !disabled.contains(&s.name))
        .filter_map(|s| Some((s.name.clone(), s.meta.blueprint.clone()?)))
        .collect()
}

/// The profile's whole catalog.
#[must_use]
pub fn catalog(scope: &ProfileScope, launch: &LaunchEnv) -> Vec<Blueprint> {
    let mut all = builtin::catalog();
    for (plugin, value) in plugin_blueprints(scope) {
        let origin = Origin::Plugin { id: plugin.clone() };
        push(
            &mut all,
            Blueprint::declared(value, &plugin, "blueprint", origin),
        );
    }
    for (skill, value) in skill_blueprints(scope, launch) {
        let origin = Origin::Skill {
            name: skill.clone(),
        };
        push(
            &mut all,
            Blueprint::declared(value, "skill", &skill, origin),
        );
    }
    all
}

/// Offers each installed skill's blueprint as a suggestion (a skill never
/// schedules anything by itself; a known key is not offered again).
///
/// # Errors
///
/// Store errors.
pub fn offer_skill_blueprints(
    store: &Store,
    scope: &ProfileScope,
    launch: &LaunchEnv,
) -> Result<(), pmt_core::error::Error> {
    for blueprint in catalog(scope, launch) {
        if matches!(blueprint.origin, Origin::Skill { .. }) {
            let suggestion =
                Suggestion::of_blueprint(&blueprint, &blueprint.key, Source::Blueprint);
            suggest::offer(store, &suggestion)?;
        }
    }
    Ok(())
}

/// The starter a newly connected account suggests, by connector name.
#[must_use]
pub fn integration_starter(connector: &str) -> Option<&'static str> {
    let name = connector.to_ascii_lowercase();
    if ["mail", "outlook", "inbox"]
        .iter()
        .any(|k| name.contains(k))
    {
        Some("important-mail")
    } else if name.contains("calendar") {
        Some("workday-start")
    } else {
        None
    }
}

/// Offers the starter a newly connected `connector` account suggests.
///
/// # Errors
///
/// Store errors.
pub fn offer_integration(store: &Store, connector: &str) -> Result<bool, pmt_core::error::Error> {
    let Some(key) = integration_starter(connector) else {
        return Ok(false);
    };
    let catalog = builtin::catalog();
    let Some(blueprint) = catalog.iter().find(|b| b.key == key) else {
        return Ok(false);
    };
    let mut suggestion = Suggestion::of_blueprint(
        blueprint,
        &format!("integration:{connector}"),
        Source::Integration,
    );
    suggestion.title = format!("{} ({connector})", blueprint.title);
    suggest::offer(store, &suggestion)
}

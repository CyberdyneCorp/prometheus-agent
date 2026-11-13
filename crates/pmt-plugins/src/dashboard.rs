//! Dashboard plugin contributions (spec: web-dashboard, Dashboard plugin
//! contributions): the pages and slots the profile's enabled plugins
//! declare in `[dashboard]`, in load order, with the override rule: when
//! two plugins override the same built-in route the first in load order
//! wins and every other one is reported as ignored.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::discovery::Discovered;
use crate::load::order::{self, Step};
use crate::load::{State, Status};
use crate::manifest::dashboard::DashboardDeclaration;

/// One plugin's contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Contribution {
    /// The plugin id.
    pub id: String,
    /// The label of its tab and frames.
    pub label: String,
    /// Its icon name.
    pub icon: Option<String>,
    /// Its page, when it has one.
    pub tab: Option<TabView>,
    /// The slots it fills.
    pub slots: Vec<String>,
    /// It serves backend routes (`ui-panel` world).
    pub backend: bool,
    /// The HTML file its frames load.
    #[serde(skip)]
    pub entry: PathBuf,
}

/// A tab as the dashboard renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabView {
    /// Its route.
    pub path: String,
    /// `end`, `after:<page>` or `before:<page>`.
    pub position: String,
    /// The built-in route it replaces, when its override won.
    #[serde(rename = "override")]
    pub override_route: Option<String>,
    /// Routable without a navigation entry.
    pub hidden: bool,
}

/// An override that lost to an earlier plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ignored {
    /// The plugin whose override was ignored.
    pub plugin: String,
    /// The route both override.
    pub route: String,
    /// The plugin that renders it.
    pub winner: String,
}

/// The contributions of a profile's plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Contributions {
    /// In load order.
    pub plugins: Vec<Contribution>,
    /// Overrides that were not applied.
    pub ignored: Vec<Ignored>,
}

impl Contributions {
    /// The contribution of plugin `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Contribution> {
        self.plugins.iter().find(|p| p.id == id)
    }
}

fn contribution(
    id: &str,
    dir: &std::path::Path,
    declared: &DashboardDeclaration,
    backend: bool,
) -> Contribution {
    Contribution {
        id: id.to_owned(),
        label: declared.label.clone(),
        icon: declared.icon.clone(),
        tab: declared.tab.as_ref().map(|tab| TabView {
            path: tab.path.clone(),
            position: tab.position.as_text(),
            override_route: tab.override_route.clone(),
            hidden: tab.hidden,
        }),
        slots: declared.slots.clone(),
        backend,
        entry: dir.join(&declared.entry),
    }
}

/// Applies the override rule to `plugins` (in load order).
#[must_use]
pub fn resolve(mut plugins: Vec<Contribution>) -> Contributions {
    let mut winners: BTreeMap<String, String> = BTreeMap::new();
    let mut ignored = Vec::new();
    for plugin in &mut plugins {
        let Some(tab) = plugin.tab.as_mut() else {
            continue;
        };
        let Some(route) = tab.override_route.clone() else {
            continue;
        };
        match winners.get(&route) {
            Some(winner) => {
                ignored.push(Ignored {
                    plugin: plugin.id.clone(),
                    route,
                    winner: winner.clone(),
                });
                tab.override_route = None;
            }
            None => {
                winners.insert(route, plugin.id.clone());
            }
        }
    }
    Contributions { plugins, ignored }
}

/// The contributions of the plugins `surveyed` (the profile's discovery)
/// would load: enabled and approved ones declaring `[dashboard]`, in load
/// order (dependencies first, then by id).
#[must_use]
pub fn contributions(surveyed: &[(Discovered, Status)]) -> Contributions {
    let declaring: BTreeMap<&str, (&Discovered, &DashboardDeclaration)> = surveyed
        .iter()
        .filter(|(_, status)| status.state == State::Loaded)
        .filter_map(|(found, status)| {
            let manifest = found.manifest.as_ref().ok()?;
            let declared = manifest.dashboard.as_ref()?;
            found.dir.as_ref()?;
            Some((status.id.as_str(), (found, declared)))
        })
        .collect();
    let wanted: BTreeMap<String, Vec<String>> = declaring
        .iter()
        .map(|(id, (found, _))| {
            let requires = found
                .manifest
                .as_ref()
                .map(|m| m.requires_plugins.clone())
                .unwrap_or_default();
            ((*id).to_owned(), requires)
        })
        .collect();
    // Dependencies outside the declaring set do not change the order.
    let wanted: BTreeMap<String, Vec<String>> = wanted
        .iter()
        .map(|(id, requires)| {
            let inside = requires
                .iter()
                .filter(|r| wanted.contains_key(*r))
                .cloned()
                .collect();
            (id.clone(), inside)
        })
        .collect();
    let ordered = order::plan(&wanted)
        .into_iter()
        .filter_map(|step| match step {
            Step::Load(id) => Some(id),
            Step::Refuse(..) => None,
        })
        .filter_map(|id| {
            let (found, declared) = declaring.get(id.as_str())?;
            let manifest = found.manifest.as_ref().ok()?;
            let backend = manifest.worlds.iter().any(|(w, _)| w.name == "ui-panel");
            Some(contribution(&id, found.dir.as_deref()?, declared, backend))
        })
        .collect();
    resolve(ordered)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Contribution, TabView, resolve};

    fn overriding(id: &str, route: &str) -> Contribution {
        Contribution {
            id: id.to_owned(),
            label: id.to_uppercase(),
            icon: None,
            tab: Some(TabView {
                path: format!("/{id}"),
                position: "end".into(),
                override_route: Some(route.to_owned()),
                hidden: false,
            }),
            slots: Vec::new(),
            backend: false,
            entry: PathBuf::new(),
        }
    }

    /// Scenario: Override conflict.
    #[test]
    fn the_first_override_in_load_order_wins_and_the_other_is_reported() {
        let resolved = resolve(vec![
            overriding("a", "/sessions"),
            overriding("b", "/sessions"),
            overriding("c", "/logs"),
        ]);
        let routes: Vec<Option<&str>> = resolved
            .plugins
            .iter()
            .map(|p| p.tab.as_ref().unwrap().override_route.as_deref())
            .collect();
        assert_eq!(routes, [Some("/sessions"), None, Some("/logs")]);
        assert_eq!(resolved.ignored.len(), 1);
        assert_eq!(
            (
                resolved.ignored[0].plugin.as_str(),
                resolved.ignored[0].route.as_str(),
                resolved.ignored[0].winner.as_str()
            ),
            ("b", "/sessions", "a")
        );
    }
}

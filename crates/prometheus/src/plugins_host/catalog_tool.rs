//! The host side of `catalog_manage` (spec: plugin-system, Catalog tool
//! for agent-initiated installs): searches the plugin catalog and the
//! official skill catalog, and installs what the user approved the way
//! `prometheus plugins install <id>` and `prometheus skills install <id>`
//! do. The model chooses nothing but the item: a plugin installs from its
//! catalog entry at the pinned ref with a verified signature and the
//! threat scan, disabled and without grants (the user enables it and
//! approves its capabilities); a skill goes through the hub's guard.

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_tools::catalog_manage::{CatalogHost, Hit, Item, Kind, Manager};
use pmt_tools::search::RemoteFuture;

/// The catalog host of one profile.
pub struct Host {
    scope: ProfileScope,
    launch: LaunchEnv,
}

/// The session's `catalog_manage`; `cards`: its surface draws the
/// approval card (desktop, TUI).
#[must_use]
pub fn manager(scope: &ProfileScope, launch: &LaunchEnv, cards: bool) -> Arc<Manager> {
    let host = Host {
        scope: scope.clone(),
        launch: launch.clone(),
    };
    Arc::new(Manager::new(Arc::new(host), cards))
}

impl Host {
    fn plugins(&self, query: &str) -> Vec<Hit> {
        let catalog = super::catalog::current(&self.scope, true);
        let installed = self.scope.dir().join("plugins");
        catalog
            .search(query)
            .into_iter()
            .filter(|e| catalog.blocked(&e.id, Some(&e.repo)).is_none())
            .map(|e| Hit {
                kind: Kind::Plugin,
                id: e.id.clone(),
                display_name: if e.description.is_empty() {
                    e.id.clone()
                } else {
                    format!("{} - {}", e.id, e.description)
                },
                tier: e.tier.clone(),
                installed: installed.join(&e.id).is_dir(),
            })
            .collect()
    }

    fn skills(&self, query: &str) -> Vec<Hit> {
        let Ok(hub) = crate::skills_cmd::hub::hub_for(&self.scope, &self.launch) else {
            return Vec::new();
        };
        let skills = self.scope.dir().join("skills");
        pmt_skillhub::search::search(&hub, query, Some("official"))
            .into_iter()
            .map(|l| Hit {
                kind: Kind::Skill,
                installed: skill_installed(&skills, &l.name),
                display_name: if l.description.is_empty() {
                    l.name.clone()
                } else {
                    format!("{} - {}", l.name, l.description)
                },
                id: l.identifier,
                tier: pmt_skillhub::sources::trust_text(l.trust).to_owned(),
            })
            .collect()
    }

    fn install_now(&self, item: &Item) -> Result<String, String> {
        match item.kind {
            Kind::Plugin => {
                crate::plugins_cmd::install::install_from_catalog(&self.scope, &item.id)
                    .map_err(|e| e.message().to_owned())?;
                Ok(format!(
                    "installed disabled; the user enables it with `prometheus plugins enable {}`",
                    item.id
                ))
            }
            Kind::Skill => crate::skills_cmd::hub::install_for(&self.scope, &self.launch, &item.id)
                .map(|name| format!("installed skill '{name}'"))
                .map_err(|e| e.message().to_owned()),
        }
    }
}

/// Whether a skill named `name` is in the profile's skills directory (at
/// its top level or one category down).
fn skill_installed(skills: &std::path::Path, name: &str) -> bool {
    if skills.join(name).is_dir() {
        return true;
    }
    std::fs::read_dir(skills)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|category| category.path().join(name).is_dir())
}

impl Host {
    fn search_now(&self, kind: Option<Kind>, query: &str) -> Vec<Hit> {
        let mut hits = Vec::new();
        if kind != Some(Kind::Skill) {
            hits.extend(self.plugins(query));
        }
        if kind != Some(Kind::Plugin) {
            hits.extend(self.skills(query));
        }
        hits
    }

    fn detached(&self) -> Self {
        Self {
            scope: self.scope.clone(),
            launch: self.launch.clone(),
        }
    }
}

impl CatalogHost for Host {
    fn search<'a>(&'a self, kind: Option<Kind>, query: &'a str) -> RemoteFuture<'a, Vec<Hit>> {
        Box::pin(async move {
            let (host, query) = (self.detached(), query.to_owned());
            // Catalog and hub requests block; keep them off the turn's runtime.
            pmt_core::spawn::blocking(move || host.search_now(kind, &query))
                .await
                .unwrap_or_default()
        })
    }

    fn install<'a>(&'a self, item: &'a Item) -> RemoteFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let host = self.detached();
            let item = item.clone();
            // Fetching and staging block; keep them off the turn's runtime.
            pmt_core::spawn::blocking(move || host.install_now(&item))
                .await
                .unwrap_or_else(|e| Err(format!("the install stopped: {e}")))
        })
    }
}

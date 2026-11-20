//! What `prometheus security scan` checks: the exact package versions the
//! supply-chain advisory check already collects (bridge `bridge.lock`
//! pins, pinned `npx`/`bunx`/`uvx` MCP servers, managed runtimes), plus
//! the plugins' dependency manifests (`requirements.txt` pins,
//! `package-lock.json`).

use std::collections::BTreeSet;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_update::advisories::Ecosystem;

/// One package at one version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Package {
    /// Where it comes from (`python environment …`, `mcp server …`,
    /// `runtime uv`, `plugin …`).
    pub component: String,
    /// The OSV ecosystem (`PyPI`, `npm`).
    pub ecosystem: &'static str,
    /// The package name.
    pub name: String,
    /// The exact version.
    pub version: String,
}

/// Which components to leave out.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // one flag per command-line option
pub(super) struct Skip {
    /// `--skip-runtimes`.
    pub runtimes: bool,
    /// `--skip-plugins` (plugin manifests and bridge environments).
    pub plugins: bool,
    /// `--skip-mcp`.
    pub mcp: bool,
}

/// Managed runtimes that are packages of an OSV ecosystem (the others,
/// Node, CPython, Chromium..., are not indexed there).
const RUNTIME_PACKAGES: &[(&str, &str, &str)] = &[("uv", "PyPI", "uv")];

/// Every package of `scope` (and its home's runtime store).
#[must_use]
pub(super) fn collect(scope: &ProfileScope, skip: Skip) -> Vec<Package> {
    let mut found = BTreeSet::new();
    for installed in crate::advisories::installed(scope) {
        let mcp = installed.location.starts_with("mcp server");
        let (wanted, ecosystem, name) = match installed.ecosystem {
            Ecosystem::Npm => (!skip.mcp, "npm", installed.name.clone()),
            Ecosystem::Pypi => (
                if mcp { !skip.mcp } else { !skip.plugins },
                "PyPI",
                installed.name.clone(),
            ),
            Ecosystem::Runtime => {
                match RUNTIME_PACKAGES.iter().find(|(n, ..)| *n == installed.name) {
                    Some((_, ecosystem, package)) => {
                        (!skip.runtimes, *ecosystem, (*package).to_owned())
                    }
                    None => continue,
                }
            }
            Ecosystem::Plugin => continue,
        };
        if wanted {
            found.insert(Package {
                component: installed.location,
                ecosystem,
                name,
                version: installed.version,
            });
        }
    }
    if !skip.plugins {
        plugin_manifests(&scope.path("plugins"), &mut found);
    }
    found.into_iter().collect()
}

/// `requirements.txt` pins (`name==version`) and `package-lock.json`
/// packages of each plugin.
fn plugin_manifests(root: &Path, out: &mut BTreeSet<Package>) {
    let Ok(plugins) = std::fs::read_dir(root) else {
        return;
    };
    for plugin in plugins.flatten() {
        let id = plugin.file_name().to_string_lossy().into_owned();
        let dir = plugin.path();
        let component = format!("plugin {id}");
        if let Ok(text) = std::fs::read_to_string(dir.join("requirements.txt")) {
            for (name, version) in text.lines().filter_map(pinned_pypi) {
                out.insert(Package {
                    component: component.clone(),
                    ecosystem: "PyPI",
                    name,
                    version,
                });
            }
        }
        if let Ok(text) = std::fs::read_to_string(dir.join("package-lock.json")) {
            for (name, version) in npm_lock(&text) {
                out.insert(Package {
                    component: component.clone(),
                    ecosystem: "npm",
                    name,
                    version,
                });
            }
        }
    }
}

/// `name==version` (extras and markers dropped).
fn pinned_pypi(line: &str) -> Option<(String, String)> {
    let line = line.split('#').next()?.split(';').next()?.trim();
    let (name, version) = line.split_once("==")?;
    let name = name.split('[').next()?.trim();
    (!name.is_empty() && !version.trim().is_empty())
        .then(|| (name.to_owned(), version.trim().to_owned()))
}

/// Packages of a lockfile v2/v3 (`packages."node_modules/<name>".version`).
fn npm_lock(text: &str) -> Vec<(String, String)> {
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(packages) = lock["packages"].as_object() else {
        return Vec::new();
    };
    packages
        .iter()
        .filter_map(|(path, entry)| {
            let name = path
                .rsplit("node_modules/")
                .next()
                .filter(|n| !n.is_empty())?;
            Some((name.to_owned(), entry["version"].as_str()?.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{npm_lock, pinned_pypi};

    #[test]
    fn manifests_give_exact_versions_only() {
        assert_eq!(
            pinned_pypi("requests[socks]==2.31.0 ; python_version > '3'"),
            Some(("requests".to_owned(), "2.31.0".to_owned()))
        );
        assert_eq!(pinned_pypi("requests>=2"), None);
        let lock = r#"{"packages": {"": {"version": "1.0.0"}, "node_modules/left-pad": {"version": "1.3.0"},
                       "node_modules/a/node_modules/@scope/b": {"version": "2.0.0"}}}"#;
        let mut found = npm_lock(lock);
        found.sort();
        assert_eq!(
            found,
            [
                ("@scope/b".to_owned(), "2.0.0".to_owned()),
                ("left-pad".to_owned(), "1.3.0".to_owned())
            ]
        );
    }
}

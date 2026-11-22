//! Where plugins come from and whether they may run (spec: plugin-system,
//! Discovery sources and identity; Opt-in enablement).
//!
//! Sources, in increasing precedence: plugins embedded in the binary,
//! `<profile_home>/plugins/<id>/`, and `<root>/.prometheus/plugins/<id>/`
//! for a project root listed in `plugins.trusted_project_dirs` that holds
//! the session's working directory. Any other project's plugin directory is
//! never read. A non-embedded plugin may not take a built-in plugin's id;
//! a project plugin shadows a user plugin of the same id and the shadowed
//! copy is reported. Nothing discovered runs until its id is in
//! `plugins.enabled` (and not in `plugins.disabled`).

use std::path::{Path, PathBuf};

use pmt_config::schema::PluginsConfig;
use pmt_core::error::{Error, codes};
use semver::Version;

use crate::digest;
use crate::manifest::Manifest;

/// A first-party plugin compiled into the binary.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedPlugin {
    /// Its `plugin.toml`.
    pub manifest: &'static str,
    /// Its component (`wasm` kind).
    pub component: &'static [u8],
    /// Its source files (`python` kind): path relative to the plugin
    /// directory, and content. They are written once per digest under the
    /// profile ([`crate::embedded::materialize`]) before the bridge hosts
    /// the plugin.
    pub files: &'static [(&'static str, &'static str)],
    /// Runs unless listed in `plugins.disabled`.
    pub enabled_by_default: bool,
}

/// Where a plugin was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Compiled into the binary.
    Embedded,
    /// `<profile_home>/plugins/<id>/`.
    User,
    /// `<root>/.prometheus/plugins/<id>/` of a trusted project root.
    Project(PathBuf),
    /// A local directory `prometheus plugins dev` serves (listed as
    /// `dev:<id>`, never installed or signed).
    Dev,
}

impl Source {
    /// `embedded`, `user` or `project`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::User => "user",
            Self::Project(_) => "project",
            Self::Dev => "dev",
        }
    }
}

/// One discovered plugin.
#[derive(Debug)]
pub struct Discovered {
    /// Its id (the directory name for an unreadable manifest).
    pub id: String,
    /// Where it was found.
    pub source: Source,
    /// Its directory (`None` for an embedded component; an embedded
    /// plugin with source files names where they are written).
    pub dir: Option<PathBuf>,
    /// Its identity digest, when its files could be read.
    pub digest: Option<String>,
    /// The manifest, or why it was refused.
    pub manifest: Result<Manifest, Error>,
    /// The embedded component bytes.
    pub embedded: Option<EmbeddedPlugin>,
    /// Lower-precedence copies of the same id it hides.
    pub shadowed: Vec<PathBuf>,
}

/// What discovery reads.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The profile's home directory.
    pub profile_dir: &'a Path,
    /// The session's working directory (project plugins); `None` scans no
    /// project.
    pub cwd: Option<&'a Path>,
    /// `[plugins]`.
    pub config: &'a PluginsConfig,
    /// The built-in plugins.
    pub embedded: &'a [EmbeddedPlugin],
    /// The running Prometheus version.
    pub running: &'a Version,
    /// Directories served by `prometheus plugins dev` (empty outside the
    /// processes dev plugins may load in).
    pub dev: &'a [PathBuf],
}

/// The id prefix of a plugin `prometheus plugins dev` serves.
pub const DEV_PREFIX: &str = "dev:";

/// Discovers every plugin visible to a session, sorted by id.
#[must_use]
pub fn discover(inputs: &Inputs<'_>) -> Vec<Discovered> {
    let mut found: Vec<Discovered> = inputs
        .embedded
        .iter()
        .map(|e| embedded(e, inputs.profile_dir, inputs.running))
        .collect();
    let mut dirs = vec![(Source::User, inputs.profile_dir.join("plugins"))];
    if let Some(root) = inputs.cwd.and_then(|cwd| trusted_root(inputs.config, cwd)) {
        dirs.push((
            Source::Project(root.clone()),
            root.join(".prometheus").join("plugins"),
        ));
    }
    for (source, dir) in dirs {
        for plugin_dir in subdirectories(&dir) {
            add(
                &mut found,
                scan(&plugin_dir, source.clone(), inputs.running),
            );
        }
    }
    for dir in inputs.dev {
        add_dev(&mut found, scan_dev(dir, inputs.running));
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// Reads a dev directory: its id is the manifest's, prefixed `dev:`.
fn scan_dev(dir: &Path, running: &Version) -> Discovered {
    let manifest = Manifest::read_any(dir, running);
    let digest = digest::of_dir(dir);
    let id = manifest.as_ref().map_or_else(
        |_| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        },
        |m| m.id.clone(),
    );
    let (digest, manifest) = match (digest, manifest) {
        (Ok(d), m) => (Some(d), m),
        (Err(e), Ok(_)) | (Err(_), Err(e)) => (None, Err(e)),
    };
    Discovered {
        id: format!("{DEV_PREFIX}{id}"),
        source: Source::Dev,
        dir: Some(dir.to_path_buf()),
        digest,
        manifest,
        embedded: None,
        shadowed: Vec::new(),
    }
}

/// Adds a dev plugin, which hides an installed plugin of the same id for
/// the session; a built-in id stays reserved.
fn add_dev(found: &mut Vec<Discovered>, mut plugin: Discovered) {
    let id = plugin.id.trim_start_matches(DEV_PREFIX).to_owned();
    if let Some(at) = found.iter().position(|d| d.id == id) {
        if found[at].source == Source::Embedded {
            plugin.manifest = Err(Error::new(
                &codes::PLUGINS_DISCOVERY,
                format!("plugin id '{id}' is reserved by a built-in plugin"),
            ));
        } else if let Some(dir) = found.remove(at).dir {
            plugin.shadowed.push(dir);
        }
    }
    found.push(plugin);
}

/// The trusted project root holding `cwd`, if any.
fn trusted_root(config: &PluginsConfig, cwd: &Path) -> Option<PathBuf> {
    let cwd = std::fs::canonicalize(cwd).ok()?;
    config
        .trusted_project_dirs
        .iter()
        .filter_map(|dir| std::fs::canonicalize(dir).ok())
        .find(|root| cwd.starts_with(root))
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// Adds `plugin`, resolving an id clash: built-in ids are reserved, a
/// project copy shadows a user copy.
fn add(found: &mut Vec<Discovered>, mut plugin: Discovered) {
    let Some(existing) = found.iter_mut().find(|d| d.id == plugin.id) else {
        found.push(plugin);
        return;
    };
    if existing.source == Source::Embedded {
        plugin.manifest = Err(Error::new(
            &codes::PLUGINS_DISCOVERY,
            format!("plugin id '{}' is reserved by a built-in plugin", plugin.id),
        ));
        // Reported under its directory, never in place of the built-in one.
        plugin.id = format!("{} ({})", plugin.id, plugin.source.as_str());
        found.push(plugin);
        return;
    }
    // Only user then project reach here: the project copy wins.
    if let Some(dir) = existing.dir.take() {
        plugin.shadowed.push(dir);
    }
    *existing = plugin;
}

fn embedded(plugin: &EmbeddedPlugin, profile_dir: &Path, running: &Version) -> Discovered {
    let manifest = Manifest::parse(plugin.manifest, running);
    let id = manifest
        .as_ref()
        .map_or_else(|_| "<built-in>".to_owned(), |m| m.id.clone());
    let digest = crate::embedded::digest(plugin);
    // A plugin with source files runs from their copy in the profile.
    let dir = (!plugin.files.is_empty()).then(|| crate::embedded::dir(profile_dir, &id, &digest));
    Discovered {
        id,
        source: Source::Embedded,
        dir,
        digest: Some(digest),
        manifest,
        embedded: Some(*plugin),
        shadowed: Vec::new(),
    }
}

/// Reads one plugin directory.
fn scan(dir: &Path, source: Source, running: &Version) -> Discovered {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let manifest = read_manifest(dir, &name, running);
    let digest = digest::of_dir(dir);
    let (digest, manifest) = match (digest, manifest) {
        (Ok(d), m) => (Some(d), m),
        (Err(e), Ok(_)) | (Err(_), Err(e)) => (None, Err(e)),
    };
    Discovered {
        id: name,
        source,
        dir: Some(dir.to_path_buf()),
        digest,
        manifest,
        embedded: None,
        shadowed: Vec::new(),
    }
}

fn read_manifest(dir: &Path, name: &str, running: &Version) -> Result<Manifest, Error> {
    // A Hermes plugin.yaml is hosted by the python bridge, named after its
    // directory.
    let manifest = Manifest::read_any(dir, running)?;
    if manifest.id != name {
        return Err(Error::new(
            &codes::PLUGINS_DISCOVERY,
            format!(
                "plugin id '{}' does not match its directory name '{name}'",
                manifest.id
            ),
        ));
    }
    Ok(manifest)
}

/// Whether a discovered plugin may be instantiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enablement {
    /// It runs.
    Enabled,
    /// It does not (the default for non-embedded plugins).
    Disabled,
    /// Listed in both lists: disabled.
    Conflict,
}

impl Enablement {
    /// Resolves `id` against `plugins.enabled` and `plugins.disabled`.
    #[must_use]
    pub fn of(plugin: &Discovered, config: &PluginsConfig) -> Self {
        // A dev plugin runs while `prometheus plugins dev` serves it.
        if plugin.source == Source::Dev {
            return Self::Enabled;
        }
        let id = &plugin.id;
        let listed = |list: &[String]| list.iter().any(|x| x == id);
        let enabled =
            listed(&config.enabled) || plugin.embedded.is_some_and(|e| e.enabled_by_default);
        match (enabled, listed(&config.disabled)) {
            (true, true) if listed(&config.enabled) => Self::Conflict,
            (true, false) => Self::Enabled,
            _ => Self::Disabled,
        }
    }

    /// The note `prometheus plugins list` prints for a conflict.
    pub const CONFLICT_NOTE: &str =
        "listed in both plugins.enabled and plugins.disabled; disabled wins";
}

#[cfg(test)]
mod tests;

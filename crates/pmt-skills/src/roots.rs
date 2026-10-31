//! The skill search roots in their one precedence order (spec: skills,
//! Search roots and single precedence order; Project-local skills require
//! explicit trust).
//!
//! Trusted project skills, then the profile's `skills/`, then each
//! `skills.external_dirs` entry, then plugin skills, then the bundled
//! tree. A cloned repository is attacker-controlled, so its
//! `.prometheus/skills/` and `.agents/skills/` load only when the git root
//! is listed in `skills.trusted_project_dirs`; otherwise the user sees one
//! notice with the command that trusts it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::schema::SkillsConfig;

use crate::bundled::Bundled;
use crate::layout;

/// A search tier, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// A trusted project's skills.
    Project,
    /// `<profile>/skills/`.
    Profile,
    /// A `skills.external_dirs` entry.
    External,
    /// A plugin's skills (`<plugin>:<skill>`).
    Plugin,
    /// Shipped with Prometheus.
    Bundled,
}

impl Tier {
    /// The name shown as a skill's `source`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Profile => "profile",
            Self::External => "external",
            Self::Plugin => "plugin",
            Self::Bundled => "bundled",
        }
    }
}

/// Where a root's files are.
#[derive(Debug, Clone)]
pub enum RootDir {
    /// A directory on disk.
    Disk(PathBuf),
    /// The embedded tree.
    Bundled(Arc<Bundled>),
}

/// One root.
#[derive(Debug, Clone)]
pub struct Root {
    /// Its tier.
    pub tier: Tier,
    /// Its files.
    pub dir: RootDir,
    /// The plugin that contributes it (plugin tier only).
    pub plugin: Option<String>,
}

/// The project-local skill directories below a git root.
pub const PROJECT_DIRS: [&str; 2] = [".prometheus/skills", ".agents/skills"];

/// Most directory levels walked up looking for `.git`.
const MAX_PROJECT_WALK: usize = 64;

/// What the roots are computed from.
#[derive(Debug, Clone)]
pub struct RootInputs<'a> {
    /// `[skills]`.
    pub config: &'a SkillsConfig,
    /// The profile directory.
    pub profile_dir: &'a Path,
    /// The session's working directory.
    pub cwd: Option<&'a Path>,
    /// The user's home (`~` expansion; never a project root).
    pub user_home: Option<&'a Path>,
    /// Plugin skill directories, `(plugin id, directory)` (M2 plugins).
    pub plugins: &'a [(String, PathBuf)],
    /// The bundled tree.
    pub bundled: Arc<Bundled>,
}

/// The resolved roots.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    /// In precedence order.
    pub roots: Vec<Root>,
    /// Notices for the user (an untrusted project with skills).
    pub notices: Vec<String>,
}

/// The profile skills directory.
#[must_use]
pub fn profile_skills(profile_dir: &Path) -> PathBuf {
    profile_dir.join("skills")
}

/// Expands a leading `~` against `user_home`.
#[must_use]
pub fn expand(path: &str, user_home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), user_home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

/// The nearest ancestor of `cwd` containing `.git`, never `user_home`
/// itself, at most 64 levels up.
#[must_use]
pub fn project_root(cwd: &Path, user_home: Option<&Path>) -> Option<PathBuf> {
    let home = user_home.and_then(|h| h.canonicalize().ok());
    let start = cwd.canonicalize().ok()?;
    for dir in start.ancestors().take(MAX_PROJECT_WALK) {
        if home.as_deref() == Some(dir) {
            return None;
        }
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Whether `root` is listed in `skills.trusted_project_dirs`.
#[must_use]
pub fn trusted(config: &SkillsConfig, root: &Path, user_home: Option<&Path>) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    config.trusted_project_dirs.iter().any(|entry| {
        let entry = expand(entry, user_home);
        entry.canonicalize().unwrap_or(entry) == root
    })
}

/// The notice for `count` skills of an untrusted project at `root`.
#[must_use]
pub fn untrusted_notice(count: usize, root: &Path) -> String {
    let noun = if count == 1 { "skill" } else { "skills" };
    format!(
        "{count} project {noun} not loaded: run prometheus skills trust {}",
        root.display()
    )
}

impl Roots {
    /// The roots of a session.
    #[must_use]
    pub fn resolve(inputs: &RootInputs<'_>) -> Self {
        let mut roots = Self::default();
        roots.project(inputs);
        roots.push(Tier::Profile, profile_skills(inputs.profile_dir), None);
        for entry in &inputs.config.external_dirs {
            let dir = expand(entry, inputs.user_home);
            if dir.is_dir() {
                roots.push(Tier::External, dir, None);
            } else {
                tracing::warn!(dir = %dir.display(), "skills.external_dirs entry does not exist; skipped");
            }
        }
        for (plugin, dir) in inputs.plugins {
            roots.push(Tier::Plugin, dir.clone(), Some(plugin.clone()));
        }
        if inputs.config.include_bundled {
            roots.roots.push(Root {
                tier: Tier::Bundled,
                dir: RootDir::Bundled(Arc::clone(&inputs.bundled)),
                plugin: None,
            });
        }
        roots
    }

    fn push(&mut self, tier: Tier, dir: PathBuf, plugin: Option<String>) {
        self.roots.push(Root {
            tier,
            dir: RootDir::Disk(dir),
            plugin,
        });
    }

    fn project(&mut self, inputs: &RootInputs<'_>) {
        if !inputs.config.project_discovery {
            return;
        }
        let Some(root) = inputs
            .cwd
            .and_then(|cwd| project_root(cwd, inputs.user_home))
        else {
            return;
        };
        let dirs: Vec<PathBuf> = PROJECT_DIRS
            .iter()
            .map(|d| root.join(d))
            .filter(|d| d.is_dir())
            .collect();
        if trusted(inputs.config, &root, inputs.user_home) {
            for dir in dirs {
                self.push(Tier::Project, dir, None);
            }
            return;
        }
        let count: usize = dirs
            .iter()
            .map(|d| layout::skill_dirs(layout::walk(d).iter().map(String::as_str)).len())
            .sum();
        if count > 0 {
            self.notices.push(untrusted_notice(count, &root));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_against_the_user_home() {
        let home = Path::new("/home/u");
        assert_eq!(expand("~/skills", Some(home)), Path::new("/home/u/skills"));
        assert_eq!(expand("~", Some(home)), Path::new("/home/u"));
        assert_eq!(expand("~other/x", Some(home)), Path::new("~other/x"));
    }

    #[test]
    fn notice_counts_skills() {
        let root = Path::new("/r");
        assert_eq!(
            untrusted_notice(1, root),
            "1 project skill not loaded: run prometheus skills trust /r"
        );
        assert!(untrusted_notice(2, root).starts_with("2 project skills not loaded"));
    }
}

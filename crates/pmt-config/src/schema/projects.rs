//! The `[projects]` section (spec: projects, Repository discovery): the
//! background scan that offers git repositories for one-click project
//! creation.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `[projects]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(extend("x-apply" = "live"))]
pub struct ProjectsConfig {
    /// Scan for git repositories; unset is on for the desktop app and off
    /// for every other surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_scan_enabled: Option<bool>,
    /// The folders scanned (`~` is the user's home); empty scans the home.
    pub repo_scan_roots: Vec<String>,
    /// How many directory levels below a root are scanned.
    #[schemars(range(min = 1, max = 16))]
    pub repo_scan_max_depth: u32,
    /// Subtrees never scanned (`~` is the user's home).
    pub repo_scan_exclude_paths: Vec<String>,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            repo_scan_enabled: None,
            repo_scan_roots: Vec::new(),
            repo_scan_max_depth: 4,
            repo_scan_exclude_paths: Vec::new(),
        }
    }
}

impl ProjectsConfig {
    /// Whether a request from `surface` sees discovered repositories.
    #[must_use]
    pub fn scans_for(&self, desktop: bool) -> bool {
        self.repo_scan_enabled.unwrap_or(desktop)
    }
}

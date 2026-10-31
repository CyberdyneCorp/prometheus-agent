//! Validated toolset selection (spec: tool-system, Toolset selection is
//! validated; Built-in toolsets and composition).
//!
//! A session's tools are the members of `tools.enabled_toolsets` (default
//! `["core"]`) minus, last, the members of `tools.disabled_toolsets`, so a
//! disabled tool is removed even when a composite lists it again. An
//! unknown name in either list is a configuration error (exit 78) naming
//! the closest known toolsets; it is never ignored.
//!
//! `all` expands to every toolset except surface-gated and default-off
//! ones: default-off toolsets are exposed only when named explicitly.

use std::collections::BTreeSet;

use pmt_config::schema::{ToolsConfig, ToolsetsConfig};
use pmt_core::error::Error;

use crate::registry::ToolRegistry;

/// The keyword for every (non-gated, non-default-off) toolset.
pub const ALL: &str = "all";

/// A resolved toolset selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The toolsets as named (after `all` expansion), in order.
    pub toolsets: Vec<String>,
    /// Tools selected, in expansion order, after subtraction.
    pub tools: Vec<String>,
    /// Tools a disabled toolset removed.
    pub disabled: BTreeSet<String>,
    /// Whether a posture toolset (`coding`) was named.
    pub posture: bool,
}

impl Selection {
    /// Resolves `enabled` minus `disabled` against `registry`.
    ///
    /// # Errors
    ///
    /// `registry.unknown_name` (exit 78): `unknown toolset '<name>' (did
    /// you mean ...?)`.
    pub fn resolve(
        registry: &ToolRegistry,
        enabled: &[String],
        disabled: &[String],
    ) -> Result<Self, Error> {
        let toolsets = names(registry, enabled)?;
        let removed = names(registry, disabled)?;
        let mut tools = Vec::new();
        for name in &toolsets {
            for tool in registry.members(name)? {
                if !tools.contains(&tool) {
                    tools.push(tool);
                }
            }
        }
        let mut subtracted = BTreeSet::new();
        for name in &removed {
            subtracted.extend(registry.members(name)?);
        }
        let disabled: BTreeSet<String> = tools
            .iter()
            .filter(|t| subtracted.contains(*t))
            .cloned()
            .collect();
        tools.retain(|t| !disabled.contains(t));
        // Only an explicitly named posture toolset selects the posture.
        let posture = enabled.iter().any(|name| {
            !removed.contains(name) && registry.toolset(name).is_some_and(|t| t.posture)
        });
        Ok(Self {
            toolsets,
            tools,
            disabled,
            posture,
        })
    }

    /// The selection of a surface with no interactive channel (cron, API,
    /// one-shot `-z`, kanban workers): every tool claiming `interactive`
    /// (`clarify`) is removed as if disabled.
    #[must_use]
    pub fn without_interactive(mut self, registry: &ToolRegistry) -> Self {
        let interactive: Vec<String> = self
            .tools
            .iter()
            .filter(|name| {
                registry
                    .get(name)
                    .is_ok_and(|tool| tool.descriptor.claims.interactive)
            })
            .cloned()
            .collect();
        self.tools.retain(|name| !interactive.contains(name));
        self.disabled.extend(interactive);
        self
    }

    /// The selection `[tools]` configures.
    ///
    /// # Errors
    ///
    /// As [`Selection::resolve`].
    pub fn from_config(registry: &ToolRegistry, tools: &ToolsConfig) -> Result<Self, Error> {
        Self::resolve(registry, &tools.enabled_toolsets, &tools.disabled_toolsets)
    }

    /// Whether `tool` is selected.
    #[must_use]
    pub fn contains(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }
}

/// Checks every toolset name of `[tools]` and `[toolsets.surfaces]`
/// (what `prometheus config validate` and session start run).
///
/// # Errors
///
/// `registry.unknown_name` (exit 78) prefixed with the offending key.
pub fn validate(
    registry: &ToolRegistry,
    tools: &ToolsConfig,
    toolsets: &ToolsetsConfig,
) -> Result<(), Error> {
    let keyed = |key: String, list: &[String]| {
        names(registry, list).map_err(|err| {
            Error::new(err.code(), format!("{key}: {}", err.message()))
                .with_hint(err.hint().unwrap_or_default())
        })
    };
    keyed("tools.enabled_toolsets".into(), &tools.enabled_toolsets)?;
    keyed("tools.disabled_toolsets".into(), &tools.disabled_toolsets)?;
    for (surface, list) in &toolsets.surfaces {
        keyed(format!("toolsets.surfaces.{surface}"), list)?;
    }
    Ok(())
}

/// The named toolsets with `all` expanded and duplicates dropped.
fn names(registry: &ToolRegistry, requested: &[String]) -> Result<Vec<String>, Error> {
    let mut out: Vec<String> = Vec::new();
    for name in requested {
        let expanded: Vec<String> = if name == ALL {
            registry
                .toolsets()
                .filter(|t| !t.surface_gated && !t.default_off)
                .map(|t| t.name.clone())
                .collect()
        } else if registry.toolset(name).is_some() {
            vec![name.clone()]
        } else {
            return Err(registry.unknown_toolset(name));
        };
        for name in expanded {
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pmt_config::schema::{ToolsConfig, ToolsetsConfig};

    use super::{Selection, validate};
    use crate::registry::{Builtins, ToolRegistry};

    fn registry() -> ToolRegistry {
        ToolRegistry::new("default", Arc::new(Builtins::standard(Vec::new()).unwrap()))
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    fn select(enabled: &[&str], disabled: &[&str]) -> Selection {
        Selection::resolve(&registry(), &strings(enabled), &strings(disabled)).unwrap()
    }

    #[test]
    fn typo_in_enabled_toolsets_fails_with_the_closest_name() {
        let tools = ToolsConfig {
            enabled_toolsets: strings(&["core", "brwser"]),
            ..ToolsConfig::default()
        };
        let err = Selection::from_config(&registry(), &tools).unwrap_err();
        assert_eq!(
            err.message(),
            "unknown toolset 'brwser' (did you mean 'browser'?)"
        );
        assert_eq!(err.exit_code(), 78);
        let err = validate(&registry(), &tools, &ToolsetsConfig::default()).unwrap_err();
        assert_eq!(
            err.message(),
            "tools.enabled_toolsets: unknown toolset 'brwser' (did you mean 'browser'?)"
        );
        assert_eq!(err.exit_code(), 78);
    }

    #[test]
    fn unknown_disabled_or_surface_toolset_is_not_ignored() {
        let tools = ToolsConfig {
            disabled_toolsets: strings(&["webb"]),
            ..ToolsConfig::default()
        };
        let err = validate(&registry(), &tools, &ToolsetsConfig::default()).unwrap_err();
        assert!(
            err.message()
                .starts_with("tools.disabled_toolsets: unknown toolset 'webb'")
        );
        let surfaces = ToolsetsConfig {
            surfaces: [("cron".to_owned(), strings(&["fil"]))].into(),
        };
        let err = validate(&registry(), &ToolsConfig::default(), &surfaces).unwrap_err();
        assert!(
            err.message()
                .starts_with("toolsets.surfaces.cron: unknown toolset 'fil'")
        );
    }

    #[test]
    fn disable_wins_over_composite() {
        let selection = select(&["core"], &["browser"]);
        assert!(!selection.tools.iter().any(|t| t.starts_with("browser_")));
        assert!(selection.contains("web_search"));
        assert!(selection.disabled.contains("browser_exec"));
        // A composite re-listing a disabled toolset's tool does not bring it back.
        let selection = select(&["debugging", "core"], &["terminal"]);
        assert!(!selection.contains("terminal") && !selection.contains("process"));
        assert!(selection.contains("read_file"));
    }

    #[test]
    fn composites_expand_their_includes_in_order() {
        let selection = select(&["debugging"], &[]);
        assert_eq!(
            selection.tools,
            strings(&[
                "terminal",
                "process",
                "web_search",
                "web_extract",
                "read_file",
                "write_file",
                "patch",
                "search_files"
            ])
        );
    }

    #[test]
    fn all_skips_default_off_toolsets() {
        let selection = select(&["all"], &[]);
        assert!(selection.toolsets.contains(&"core".to_owned()));
        for off in ["x_search", "video", "video_gen", "kanban"] {
            assert!(!selection.toolsets.contains(&off.to_owned()), "{off}");
        }
        assert!(!selection.contains("x_search"));
        assert!(select(&["core", "x_search"], &[]).contains("x_search"));
    }

    #[test]
    fn posture_only_when_coding_is_named() {
        assert!(select(&["coding"], &[]).posture);
        assert!(!select(&["core"], &[]).posture);
        assert!(!select(&["coding"], &["coding"]).posture);
        assert!(!select(&["all"], &[]).posture);
    }
}

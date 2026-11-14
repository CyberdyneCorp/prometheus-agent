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
//! ones: default-off toolsets are exposed only when named explicitly. An
//! MCP server's bare name stands for its `mcp-<server>` toolset unless a
//! toolset has that name (spec: tool-system, Registry toolset aliases).
//!
//! Disabling a toolset built on `core` (a platform bundle, `gateway`,
//! `api_server`) removes only its members outside `core` (spec:
//! tool-system, Disabling a bundle never strips core).

mod surfaces;

use std::collections::BTreeSet;

use pmt_config::schema::{ToolsConfig, ToolsetsConfig};
use pmt_core::error::Error;

use crate::registry::ToolRegistry;

pub use self::surfaces::{SessionSurface, WEBHOOK};

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
    /// Selected tools this session opted into itself (`/browser connect`
    /// for `browser_cdp`): their profile-level opt-in probe does not
    /// apply, the session's own state answers it.
    pub session_opt_ins: BTreeSet<String>,
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
            let members = registry.members(name)?;
            if registry.builds_on_core(name) {
                let core = registry.members("core")?;
                subtracted.extend(members.into_iter().filter(|t| !core.contains(t)));
            } else {
                subtracted.extend(members);
            }
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
            session_opt_ins: BTreeSet::new(),
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
                registry.get(name).is_ok_and(|tool| {
                    tool.descriptor
                        .claims
                        .as_ref()
                        .is_some_and(|c| c.interactive)
                })
            })
            .cloned()
            .collect();
        self.tools.retain(|name| !interactive.contains(name));
        self.disabled.extend(interactive);
        self
    }

    /// This selection with `tools` opted in by the session itself (see
    /// [`Selection::session_opt_ins`]); unselected names are ignored.
    #[must_use]
    pub fn with_session_opt_ins<'a>(mut self, tools: impl IntoIterator<Item = &'a str>) -> Self {
        self.session_opt_ins = tools
            .into_iter()
            .filter(|name| self.contains(name))
            .map(str::to_owned)
            .collect();
        self
    }

    /// The selection of a session of `surface`: its starting toolsets
    /// ([`SessionSurface::starting_toolsets`]) minus `tools.disabled_toolsets`,
    /// with the surface's gated toolsets folded in.
    ///
    /// # Errors
    ///
    /// As [`Selection::resolve`].
    pub fn for_surface(
        registry: &ToolRegistry,
        surface: &SessionSurface,
        tools: &ToolsConfig,
        toolsets: &ToolsetsConfig,
    ) -> Result<Self, Error> {
        let enabled = surface.starting_toolsets(registry, tools, toolsets);
        Self::resolve(registry, &enabled, &tools.disabled_toolsets)?.folded_for(
            registry,
            surface,
            &tools.disabled_toolsets,
            &[],
        )
    }

    /// This selection with `surface`'s gated toolsets
    /// ([`SessionSurface::folded_toolsets`]) folded in, except one
    /// `disabled` names and the `hidden` tools the user opted out of, and
    /// with the gated toolsets of every other surface taken out even when
    /// a list named them.
    ///
    /// # Errors
    ///
    /// A gated toolset missing from `registry`.
    pub fn folded_for(
        mut self,
        registry: &ToolRegistry,
        surface: &SessionSurface,
        disabled: &[String],
        hidden: &[&str],
    ) -> Result<Self, Error> {
        let folded = surface.folded_toolsets();
        for gated in [
            crate::agent_level_desktop::DESKTOP_UI,
            crate::agent_level_desktop::ONBOARDING,
        ] {
            if folded.contains(&gated) {
                continue;
            }
            let members = registry.members(gated)?;
            self.tools.retain(|t| !members.contains(t));
            self.toolsets.retain(|t| t != gated);
        }
        self.tools.retain(|t| !hidden.contains(&t.as_str()));
        for name in folded {
            if disabled.iter().any(|d| d == name) {
                let members = registry.members(name)?;
                self.tools.retain(|t| !members.contains(t));
                continue;
            }
            if !self.toolsets.iter().any(|t| t == name) {
                self.toolsets.push((*name).to_owned());
            }
            for tool in registry.members(name)? {
                if !hidden.contains(&tool.as_str()) && !self.contains(&tool) {
                    self.tools.push(tool);
                }
            }
        }
        Ok(self)
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
    for (surface, list) in toolsets.surfaces.entries() {
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
        } else if let Some(canonical) = registry.canonical_toolset(name) {
            vec![canonical]
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
            surfaces: pmt_config::schema::SurfaceToolsets {
                cron: Some(strings(&["fil"])),
                ..Default::default()
            },
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

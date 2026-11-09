//! A child's capability grant (spec: subagent-delegation, Child capability
//! grant can only narrow; Role derived from depth only).
//!
//! Every side is first expanded to concrete tool names (composite and MCP
//! server toolsets included), then intersected with the parent's
//! effective tools, so asking for `file` under a parent on `coding` is a
//! narrowing, not a broadening. Anything asked for beyond the parent's
//! grant refuses the task listing every offending item; nothing is
//! silently dropped. Filesystem, network and secret capabilities narrow
//! the same way through [`CapabilitySet`].

use pmt_core::capability::CapabilitySet;
use pmt_core::error::codes;
use pmt_tools::ToolRegistry;

use crate::Refusal;

/// Tools a child never has: it cannot ask the user, write long-term
/// memory, message chats, schedule jobs or drive kanban boards.
pub const NEVER_FOR_CHILDREN: &[&str] = &["clarify", "memory", "send_message", "cronjob", "kanban"];

/// The tool that spawns children.
pub const DELEGATE_TOOL: &str = "delegate_task";

/// A child's role, derived from its depth alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Keeps the `delegation` toolset (depth below `max_spawn_depth`).
    Orchestrator,
    /// Spawns nothing.
    Leaf,
}

impl Role {
    /// The role of a child at `depth` (from 1) under `max_spawn_depth`.
    #[must_use]
    pub const fn at(depth: u32, max_spawn_depth: u32) -> Self {
        if depth < max_spawn_depth {
            Self::Orchestrator
        } else {
            Self::Leaf
        }
    }

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Leaf => "leaf",
        }
    }
}

/// What the parent holds.
#[derive(Debug, Clone, Default)]
pub struct ParentGrant {
    /// The tools of the parent's effective surface.
    pub tools: Vec<String>,
    /// The parent's effective capabilities.
    pub capabilities: CapabilitySet,
}

/// What one task asks for, from every narrowing side.
#[derive(Debug, Clone, Default)]
pub struct Narrowing<'a> {
    /// The task's `toolsets`.
    pub task_toolsets: Option<&'a [String]>,
    /// The agent definition's `toolsets`.
    pub agent_toolsets: Option<&'a [String]>,
    /// Capabilities a plugin launch or router asked for (`None`: the
    /// parent's).
    pub capabilities: Option<&'a CapabilitySet>,
    /// The target profile's capabilities on a profile route.
    pub profile: Option<&'a CapabilitySet>,
}

/// A child's grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The child's tools, in the parent's order.
    pub tools: Vec<String>,
    /// The child's capabilities.
    pub capabilities: CapabilitySet,
    /// Its role.
    pub role: Role,
}

/// Derives the grant of a child at `depth` from `parent` and `narrowing`.
///
/// # Errors
///
/// `delegation.grant_broadening` listing every tool or capability asked
/// for beyond the parent's grant; `delegation.invalid_task` for an
/// unknown toolset.
pub fn derive(
    registry: &ToolRegistry,
    parent: &ParentGrant,
    narrowing: &Narrowing<'_>,
    depth: u32,
    max_spawn_depth: u32,
) -> Result<Grant, Refusal> {
    let mut tools = parent.tools.clone();
    let mut beyond: Vec<String> = Vec::new();
    for requested in [narrowing.task_toolsets, narrowing.agent_toolsets]
        .into_iter()
        .flatten()
    {
        let wanted = expand(registry, requested)?;
        for tool in &wanted {
            if !parent.tools.contains(tool) && !beyond.contains(tool) {
                beyond.push(tool.clone());
            }
        }
        tools.retain(|t| wanted.contains(t));
    }
    let capabilities = match narrowing.capabilities {
        Some(requested) => {
            beyond.extend(
                parent
                    .capabilities
                    .missing(requested)
                    .iter()
                    .map(ToString::to_string),
            );
            requested.clone()
        }
        None => parent.capabilities.clone(),
    };
    if !beyond.is_empty() {
        return Err(Refusal::new(
            &codes::DELEGATION_GRANT_BROADENING,
            format!(
                "The task asks for more than this session holds: {}.",
                beyond.join(", ")
            ),
        ));
    }
    let capabilities = match narrowing.profile {
        Some(profile) => capabilities.intersect(profile),
        None => capabilities,
    };
    let role = Role::at(depth, max_spawn_depth);
    let kanban = registry.members("kanban").unwrap_or_default();
    tools.retain(|t| {
        !NEVER_FOR_CHILDREN.contains(&t.as_str())
            && !kanban.contains(t)
            && (role == Role::Orchestrator || t != DELEGATE_TOOL)
    });
    Ok(Grant {
        tools,
        capabilities,
        role,
    })
}

/// The concrete tools of `toolsets`.
fn expand(registry: &ToolRegistry, toolsets: &[String]) -> Result<Vec<String>, Refusal> {
    let mut out: Vec<String> = Vec::new();
    for name in toolsets {
        let members = registry.members(name).map_err(|err| {
            Refusal::new(&codes::DELEGATION_INVALID_TASK, err.message().to_owned())
        })?;
        for tool in members {
            if !out.contains(&tool) {
                out.push(tool);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ToolRegistry {
        ToolRegistry::new(
            "default",
            std::sync::Arc::new(
                pmt_tools::Builtins::standard(pmt_tools::agent_level::tools()).unwrap(),
            ),
        )
    }

    fn parent_on(toolsets: &[&str]) -> ParentGrant {
        let registry = registry();
        let mut tools: Vec<String> = Vec::new();
        for set in toolsets {
            for t in registry.members(set).unwrap() {
                if !tools.contains(&t) {
                    tools.push(t);
                }
            }
        }
        ParentGrant {
            tools,
            capabilities: CapabilitySet::parse(["fs.write:/work/repo/**", "net.connect:*"])
                .unwrap(),
        }
    }

    fn toolsets(names: &[&str]) -> Vec<String> {
        names.iter().map(|&s| s.to_owned()).collect()
    }

    /// Scenario: Composite narrowing is not falsely rejected.
    #[test]
    fn a_composite_parent_narrows_to_its_member_toolsets() {
        let registry = registry();
        let asked = toolsets(&["file", "terminal"]);
        let narrowing = Narrowing {
            task_toolsets: Some(&asked),
            ..Narrowing::default()
        };
        let grant = derive(&registry, &parent_on(&["coding"]), &narrowing, 1, 1).unwrap();
        let mut expected = registry.members("file").unwrap();
        expected.extend(registry.members("terminal").unwrap());
        let mut got = grant.tools.clone();
        got.sort();
        expected.sort();
        assert_eq!(got, expected);
    }

    /// Scenario: Broadening refused.
    #[test]
    fn asking_for_a_toolset_the_parent_lacks_lists_every_tool() {
        let registry = registry();
        let asked = toolsets(&["browser"]);
        let narrowing = Narrowing {
            task_toolsets: Some(&asked),
            ..Narrowing::default()
        };
        let err = derive(&registry, &parent_on(&["file"]), &narrowing, 1, 1).unwrap_err();
        assert_eq!(err.code.code, "delegation.grant_broadening");
        for tool in registry.members("browser").unwrap() {
            assert!(
                err.message.contains(&tool),
                "{tool} missing from {}",
                err.message
            );
        }
    }

    /// Scenario: Flat default yields leaf children.
    #[test]
    fn a_leaf_has_no_delegate_task_and_children_never_get_user_or_memory_tools() {
        let registry = registry();
        let parent = parent_on(&["core"]);
        assert!(parent.tools.iter().any(|t| t == DELEGATE_TOOL));
        let leaf = derive(&registry, &parent, &Narrowing::default(), 1, 1).unwrap();
        assert_eq!(leaf.role, Role::Leaf);
        for absent in [DELEGATE_TOOL, "clarify", "memory", "cronjob"] {
            assert!(!leaf.tools.iter().any(|t| t == absent), "{absent}");
        }
        let orchestrator = derive(&registry, &parent, &Narrowing::default(), 1, 2).unwrap();
        assert_eq!(orchestrator.role, Role::Orchestrator);
        assert!(orchestrator.tools.iter().any(|t| t == DELEGATE_TOOL));
    }

    #[test]
    fn capabilities_only_narrow() {
        let registry = registry();
        let parent = parent_on(&["file"]);
        let wider = CapabilitySet::parse(["fs.write:/**"]).unwrap();
        let err = derive(
            &registry,
            &parent,
            &Narrowing {
                capabilities: Some(&wider),
                ..Narrowing::default()
            },
            1,
            1,
        )
        .unwrap_err();
        assert!(err.message.contains("fs.write:/**"), "{}", err.message);
        let profile = CapabilitySet::parse(["fs.write:/work/repo/src/**"]).unwrap();
        let grant = derive(
            &registry,
            &parent,
            &Narrowing {
                profile: Some(&profile),
                ..Narrowing::default()
            },
            1,
            1,
        )
        .unwrap();
        assert_eq!(
            grant.capabilities.to_strings(),
            ["fs.write:/work/repo/src/**"]
        );
    }
}

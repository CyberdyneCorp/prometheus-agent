//! The typed tool registry, scoped per profile (spec: tool-system, Typed
//! tool registry; Shadowing is rejected unless granted; Registry is scoped
//! per profile; Built-in toolsets and composition).
//!
//! [`Builtins`] holds the built-in tools and toolsets once per process. A
//! [`ToolRegistry`] is one profile's view: the shared built-ins plus that
//! profile's overlay (its plugins, MCP servers and contributed toolsets).
//! Lookups never consult another profile's overlay. Every mutation bumps
//! the registry generation; rejected registrations are logged at ERROR
//! and kept for `prometheus doctor`.

use std::collections::BTreeMap;
use std::sync::Arc;

use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::doctor::{Check, Status};
use pmt_core::error::{Error, codes};

use crate::descriptor::{self, Owner, RegisteredTool};
use crate::toolset::{self, Toolset, Toolsets};

/// The built-in tools and toolsets, shared by every profile.
#[derive(Debug, Clone)]
pub struct Builtins {
    tools: BTreeMap<String, Arc<RegisteredTool>>,
    toolsets: Toolsets,
}

impl Builtins {
    /// The shipped toolsets and `tools`.
    ///
    /// # Errors
    ///
    /// As [`Builtins::with`].
    pub fn standard(tools: Vec<RegisteredTool>) -> Result<Self, Error> {
        Self::with(toolset::builtin(), tools)
    }

    /// `toolsets` and `tools`, checked like any registration.
    ///
    /// # Errors
    ///
    /// `toolset.cycle`, `registry.unknown_name` (an include or a tool's
    /// toolset), `tool.invalid_registration`, `registry.duplicate_name`.
    pub fn with(toolsets: Vec<Toolset>, tools: Vec<RegisteredTool>) -> Result<Self, Error> {
        toolset::check_batch(&Toolsets::new(), &toolsets)?;
        let toolsets: Toolsets = toolsets.into_iter().map(|t| (t.name.clone(), t)).collect();
        let mut map = BTreeMap::new();
        for tool in tools {
            check_tool(&tool, |name| toolsets.contains_key(name), toolsets.keys())?;
            let name = tool.name().to_owned();
            if map.insert(name.clone(), Arc::new(tool)).is_some() {
                return Err(Error::new(
                    &codes::REGISTRY_DUPLICATE_NAME,
                    format!("tool '{name}' is registered twice"),
                ));
            }
        }
        Ok(Self {
            tools: map,
            toolsets,
        })
    }
}

/// A registration the registry refused.
#[derive(Debug, Clone)]
pub struct Rejection {
    /// Who tried to register.
    pub owner: Owner,
    /// Why it was refused.
    pub message: String,
    /// The remedy, when there is one.
    pub hint: Option<String>,
}

/// One profile's tools and toolsets.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    profile: String,
    builtins: Arc<Builtins>,
    tools: BTreeMap<String, Arc<RegisteredTool>>,
    toolsets: Toolsets,
    generation: u64,
    rejections: Vec<Rejection>,
}

impl ToolRegistry {
    /// The registry of `profile` over the shared `builtins`.
    #[must_use]
    pub fn new(profile: impl Into<String>, builtins: Arc<Builtins>) -> Self {
        Self {
            profile: profile.into(),
            builtins,
            tools: BTreeMap::new(),
            toolsets: Toolsets::new(),
            generation: 0,
            rejections: Vec::new(),
        }
    }

    /// The profile this registry belongs to.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Bumped on every successful mutation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The tool registered as `name` (overlay first: a granted plugin
    /// override replaces the built-in).
    ///
    /// # Errors
    ///
    /// `tool.unknown`: `unknown tool: <name>` (exit 2).
    pub fn get(&self, name: &str) -> Result<&Arc<RegisteredTool>, Error> {
        self.find(name)
            .ok_or_else(|| Error::new(&codes::TOOL_UNKNOWN, format!("unknown tool: {name}")))
    }

    fn find(&self, name: &str) -> Option<&Arc<RegisteredTool>> {
        self.tools
            .get(name)
            .or_else(|| self.builtins.tools.get(name))
    }

    /// Every tool visible in this profile, by name.
    pub fn tools(&self) -> impl Iterator<Item = &Arc<RegisteredTool>> {
        let shadowed = |name: &String| self.tools.contains_key(name);
        self.builtins
            .tools
            .iter()
            .filter(move |(name, _)| !shadowed(name))
            .map(|(_, tool)| tool)
            .chain(self.tools.values())
    }

    /// The toolset `name`.
    #[must_use]
    pub fn toolset(&self, name: &str) -> Option<&Toolset> {
        self.toolsets
            .get(name)
            .or_else(|| self.builtins.toolsets.get(name))
    }

    /// Every toolset of this profile, by name.
    pub fn toolsets(&self) -> impl Iterator<Item = &Toolset> {
        let mut all: BTreeMap<&str, &Toolset> = self
            .builtins
            .toolsets
            .iter()
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        all.extend(self.toolsets.iter().map(|(k, v)| (k.as_str(), v)));
        all.into_values()
    }

    /// `unknown toolset '<name>' (did you mean ...?)`.
    pub(crate) fn unknown_toolset(&self, name: &str) -> Error {
        toolset::unknown(name, self.toolsets().map(|t| t.name.clone()))
    }

    /// Registers `tool`. A name already registered by another owner is
    /// refused, except that a plugin holding `tools.override:<tool>` in
    /// `grants` may replace a built-in; the same owner re-registering (an
    /// MCP reconnect) replaces its entry.
    ///
    /// # Errors
    ///
    /// `tool.invalid_registration`, `registry.unknown_name` (unknown
    /// toolset), `tool.shadowed`; each is also kept for the doctor.
    pub fn register(&mut self, tool: RegisteredTool, grants: &CapabilitySet) -> Result<(), Error> {
        let owner = tool.owner.clone();
        let checked = self.check_registration(&tool, grants);
        if let Err(error) = checked {
            return Err(self.reject(owner, error));
        }
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
        self.generation += 1;
        Ok(())
    }

    fn check_registration(
        &self,
        tool: &RegisteredTool,
        grants: &CapabilitySet,
    ) -> Result<(), Error> {
        let names: Vec<String> = self.toolsets().map(|t| t.name.clone()).collect();
        check_tool(tool, |name| self.toolset(name).is_some(), names.iter())?;
        let Some(existing) = self.find(tool.name()) else {
            return Ok(());
        };
        if existing.owner == tool.owner {
            return Ok(());
        }
        let override_grant = Capability::ToolsOverride(tool.name().to_owned());
        let may_override = existing.owner == Owner::Builtin
            && matches!(tool.owner, Owner::Plugin(_))
            && grants.allows(&override_grant);
        if may_override {
            return Ok(());
        }
        Err(Error::new(
            &codes::TOOL_SHADOWED,
            format!(
                "tool '{}' from {} refused: already registered by {}",
                tool.name(),
                tool.owner,
                existing.owner
            ),
        )
        .with_hint(format!(
            "a plugin replaces a built-in tool only with the grant {override_grant}"
        )))
    }

    /// Removes every tool and toolset `owner` registered (plugin disabled,
    /// MCP server removed).
    pub fn unregister(&mut self, owner: &Owner) {
        let before = (self.tools.len(), self.toolsets.len());
        self.tools.retain(|_, tool| &tool.owner != owner);
        self.toolsets.retain(|_, set| &set.owner != owner);
        if before != (self.tools.len(), self.toolsets.len()) {
            self.generation += 1;
        }
    }

    /// Registers the toolsets one owner contributes, all or none: an
    /// unknown include, a name taken by another owner or an include cycle
    /// rejects the whole batch.
    ///
    /// # Errors
    ///
    /// `registry.unknown_name`, `registry.duplicate_name`,
    /// `toolset.cycle`; each is also kept for the doctor.
    pub fn register_toolsets(&mut self, owner: &Owner, batch: Vec<Toolset>) -> Result<(), Error> {
        if let Err(error) = self.check_toolsets(owner, &batch) {
            return Err(self.reject(owner.clone(), error));
        }
        for mut set in batch {
            set.owner = owner.clone();
            self.toolsets.insert(set.name.clone(), set);
        }
        self.generation += 1;
        Ok(())
    }

    fn check_toolsets(&self, owner: &Owner, batch: &[Toolset]) -> Result<(), Error> {
        for set in batch {
            if let Some(existing) = self.toolset(&set.name)
                && &existing.owner != owner
            {
                return Err(Error::new(
                    &codes::REGISTRY_DUPLICATE_NAME,
                    format!(
                        "toolset '{}' from {owner} refused: already registered by {}",
                        set.name, existing.owner
                    ),
                ));
            }
        }
        let known: Toolsets = self
            .toolsets()
            .filter(|t| &t.owner != owner)
            .map(|t| (t.name.clone(), t.clone()))
            .collect();
        toolset::check_batch(&known, batch)
    }

    fn reject(&mut self, owner: Owner, error: Error) -> Error {
        tracing::error!(
            profile = %self.profile,
            code = error.code().code,
            "tool registration refused: {}",
            error.message()
        );
        self.rejections.push(Rejection {
            owner,
            message: error.message().to_owned(),
            hint: error.hint().map(str::to_owned),
        });
        error
    }

    /// Refused registrations, oldest first.
    #[must_use]
    pub fn rejections(&self) -> &[Rejection] {
        &self.rejections
    }

    /// One `tools` doctor check per refused registration, or one `Ok`.
    #[must_use]
    pub fn doctor_checks(&self) -> Vec<Check> {
        if self.rejections.is_empty() {
            return vec![Check::new(
                "tools",
                "registrations",
                Status::Ok,
                "every tool registration was accepted",
            )];
        }
        self.rejections
            .iter()
            .map(|r| {
                let check = Check::new("tools", "registrations", Status::Warn, &r.message);
                match &r.hint {
                    Some(hint) => check.with_hint(hint),
                    None => check,
                }
            })
            .collect()
    }

    /// The members of toolset `name`: listed tools, then tools registered
    /// under it, then the members of its includes; first occurrence wins.
    ///
    /// # Errors
    ///
    /// `registry.unknown_name` for an unknown toolset.
    pub fn members(&self, name: &str) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        self.expand(name, &mut out)?;
        Ok(out)
    }

    fn expand(&self, name: &str, out: &mut Vec<String>) -> Result<(), Error> {
        let set = self
            .toolset(name)
            .ok_or_else(|| self.unknown_toolset(name))?;
        let registered = self
            .tools()
            .filter(|t| t.descriptor.toolset == name)
            .map(|t| t.name().to_owned());
        for tool in set.tools.iter().cloned().chain(registered) {
            if !out.contains(&tool) {
                out.push(tool);
            }
        }
        for include in &set.includes {
            self.expand(include, out)?;
        }
        Ok(())
    }
}

/// Validates a tool's descriptor and owning toolset.
fn check_tool<'a>(
    tool: &RegisteredTool,
    has_toolset: impl Fn(&str) -> bool,
    toolsets: impl Iterator<Item = &'a String>,
) -> Result<(), Error> {
    tool.descriptor.validate(&tool.owner)?;
    let wanted = &tool.descriptor.toolset;
    if has_toolset(wanted) {
        return Ok(());
    }
    let err = toolset::unknown(wanted, toolsets.cloned());
    Err(descriptor::invalid(tool.name(), err.message()))
}

#[cfg(test)]
mod tests;

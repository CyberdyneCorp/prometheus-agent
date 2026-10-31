//! Tool descriptors and handlers (spec: tool-system, Typed tool registry;
//! design D15).
//!
//! A [`ToolDescriptor`] is everything the registry, the exposure rules, the
//! dispatcher and the batch planner need to know about a tool without
//! running it: its argument schema, owning toolset, required capabilities,
//! `effect`, `idempotent` and resource claims. A [`RegisteredTool`] pairs it
//! with how the tool executes ([`Executor`]).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pmt_core::cancel::CancelToken;
use pmt_core::capability::Capability;
use pmt_core::error::{Error, codes};
use serde::Serialize;
use serde_json::Value;

use crate::probe::Probe;
use crate::result::ToolResult;

/// The longest tool name (`^[a-z][a-z0-9_]{0,63}$`).
pub const MAX_NAME_LEN: usize = 64;

/// What running a tool can change (design D15). Crash-resume re-runs only
/// `none` or idempotent calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Reads only.
    None,
    /// Changes files inside the session workspace.
    Workspace,
    /// Changes the world outside (network writes, processes, messages).
    External,
}

/// A path a call reads or writes: fixed, or taken from an argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathClaim {
    /// The string argument at this JSON pointer (`/path`).
    Argument(String),
    /// A fixed path or glob.
    Fixed(String),
    /// The files a V4A patch (the string argument at this JSON pointer)
    /// adds, updates, deletes or moves to.
    PatchText(String),
}

/// The resources a call touches; the batch planner schedules by them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResourceClaims {
    /// Paths read.
    pub reads: Vec<PathClaim>,
    /// Paths written.
    pub writes: Vec<PathClaim>,
    /// Named resources only one call may hold at a time (`browser`,
    /// `terminal:<session>`...).
    pub exclusive: Vec<String>,
    /// The call talks to the human (it must not run beside another).
    pub interactive: bool,
}

impl ResourceClaims {
    /// Whether a call with these claims may run beside other calls: it
    /// writes nothing, holds no exclusive resource and is not interactive.
    #[must_use]
    pub fn parallel_safe(&self) -> bool {
        self.writes.is_empty() && self.exclusive.is_empty() && !self.interactive
    }
}

/// Who registered a tool or toolset.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Owner {
    /// Shipped with Prometheus.
    Builtin,
    /// A plugin, by id.
    Plugin(String),
    /// An MCP server, by configured name.
    Mcp(String),
    /// Another capability (`context_engine`, `acp`...).
    Capability(String),
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin => f.write_str("built-in"),
            Self::Plugin(id) => write!(f, "plugin '{id}'"),
            Self::Mcp(server) => write!(f, "MCP server '{server}'"),
            Self::Capability(name) => write!(f, "{name}"),
        }
    }
}

/// The static description of one tool.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    /// Unique name the model calls.
    pub name: String,
    /// Model-facing description. It must not name tools of other toolsets;
    /// cross-tool guidance belongs in [`RegisteredTool::rewriter`].
    pub description: String,
    /// JSON Schema of the arguments; the root is an object schema.
    pub parameters: Value,
    /// The toolset the tool belongs to.
    pub toolset: String,
    /// Capabilities the session must hold for the tool to be exposed.
    pub capabilities: Vec<Capability>,
    /// What the tool can change.
    pub effect: Effect,
    /// Whether repeating a call has no further effect.
    pub idempotent: bool,
    /// Resources a call touches.
    pub claims: ResourceClaims,
    /// Overrides `tools.default_timeout_secs`.
    pub timeout: Option<Duration>,
    /// The tool's own result cap in characters (see the result budget).
    pub result_cap: Option<usize>,
}

impl ToolDescriptor {
    /// A descriptor with no capabilities, `effect = external` (the safe
    /// assumption), not idempotent and without claims.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        toolset: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            toolset: toolset.into(),
            capabilities: Vec::new(),
            effect: Effect::External,
            idempotent: false,
            claims: ResourceClaims::default(),
            timeout: None,
            result_cap: None,
        }
    }

    /// Checks the name and the schema root (the toolset is checked by the
    /// registry, which knows the toolsets).
    ///
    /// # Errors
    ///
    /// `tool.invalid_registration` naming the tool and the field.
    pub fn validate(&self, owner: &Owner) -> Result<(), Error> {
        if !valid_name(&self.name, owner) {
            return Err(invalid(
                &self.name,
                "name must match ^[a-z][a-z0-9_]{0,63}$",
            ));
        }
        let object_root = self
            .parameters
            .as_object()
            .is_some_and(|schema| schema.get("type").is_none_or(|t| t == "object"));
        if !object_root {
            return Err(invalid(
                &self.name,
                "parameters schema root must be an object",
            ));
        }
        Ok(())
    }
}

/// `tool '<name>': <what>`.
pub(crate) fn invalid(name: &str, what: &str) -> Error {
    Error::new(
        &codes::TOOL_INVALID_REGISTRATION,
        format!("tool '{name}': {what}"),
    )
}

/// Built-in and plugin names follow `^[a-z][a-z0-9_]{0,63}$`; MCP names
/// (`mcp__<server>__<tool>`, already sanitized) may also use upper case.
fn valid_name(name: &str, owner: &Owner) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let mcp = matches!(owner, Owner::Mcp(_));
    let body_ok = |c: char| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || (mcp && c.is_ascii_uppercase())
    };
    name.len() <= MAX_NAME_LEN && first.is_ascii_lowercase() && chars.all(body_ok)
}

/// What a running call knows about itself.
#[derive(Debug, Clone)]
pub struct CallContext {
    /// The model's call id.
    pub call_id: String,
    /// The session the call belongs to.
    pub session_id: String,
    /// Cancelled when the user interrupts the turn.
    pub cancel: CancelToken,
}

/// A handler failure: becomes `[TOOL_ERROR] <tool> failed: <kind>: <message>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    /// A short class (`io`, `invalid_input`, `trap`...).
    pub kind: String,
    /// What went wrong.
    pub message: String,
}

impl ToolFailure {
    /// A failure of `kind`.
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

/// What a handler returns.
pub type HandlerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + 'a>>;

/// Runs one tool.
pub trait ToolHandler: Send + Sync {
    /// Executes a call with validated `arguments` (always an object).
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a>;
}

/// How a tool executes.
#[derive(Clone)]
pub enum Executor {
    /// Through the dispatcher.
    Handler(Arc<dyn ToolHandler>),
    /// Only by the turn loop, which holds the session state the tool needs
    /// (`memory`, `todo`, `session_search`, `clarify`, `delegate_task`).
    AgentLoop,
}

impl fmt::Debug for Executor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handler(_) => f.write_str("Handler"),
            Self::AgentLoop => f.write_str("AgentLoop"),
        }
    }
}

/// Rewrites a static description once the session's exposed tool names
/// are known, so cross-tool guidance names only present tools.
pub type Rewriter = Arc<dyn Fn(&str, &ExposedNames) -> String + Send + Sync>;

/// The names of the tools a session exposes.
pub type ExposedNames = std::collections::BTreeSet<String>;

/// A descriptor with its executor, availability probe and description
/// rewriter: one registry entry.
#[derive(Clone)]
pub struct RegisteredTool {
    /// The static description.
    pub descriptor: ToolDescriptor,
    /// Who registered it.
    pub owner: Owner,
    /// How it runs.
    pub executor: Executor,
    /// Reachability or opt-in check; `None` means always available.
    pub probe: Option<Arc<dyn Probe>>,
    /// Session-time description rewriter.
    pub rewriter: Option<Rewriter>,
}

impl RegisteredTool {
    /// An entry run by `handler`, without probe or rewriter.
    #[must_use]
    pub fn new(descriptor: ToolDescriptor, owner: Owner, handler: Arc<dyn ToolHandler>) -> Self {
        Self {
            descriptor,
            owner,
            executor: Executor::Handler(handler),
            probe: None,
            rewriter: None,
        }
    }

    /// An entry the turn loop executes.
    #[must_use]
    pub fn agent_loop(descriptor: ToolDescriptor) -> Self {
        Self {
            descriptor,
            owner: Owner::Builtin,
            executor: Executor::AgentLoop,
            probe: None,
            rewriter: None,
        }
    }

    /// Adds an availability probe.
    #[must_use]
    pub fn with_probe(mut self, probe: Arc<dyn Probe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Adds a description rewriter.
    #[must_use]
    pub fn with_rewriter(mut self, rewriter: Rewriter) -> Self {
        self.rewriter = Some(rewriter);
        self
    }

    /// The tool name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.descriptor.name
    }
}

impl fmt::Debug for RegisteredTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredTool")
            .field("name", &self.descriptor.name)
            .field("owner", &self.owner)
            .field("executor", &self.executor)
            .field("probe", &self.probe.as_ref().map(|p| p.id().to_owned()))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Owner, ToolDescriptor};

    fn descriptor(name: &str, parameters: serde_json::Value) -> ToolDescriptor {
        ToolDescriptor::new(name, "file", "d", parameters)
    }

    #[test]
    fn names_follow_the_pattern() {
        let object = json!({"type": "object"});
        for good in ["a", "read_file", "x9", &"a".repeat(64)] {
            assert!(
                descriptor(good, object.clone())
                    .validate(&Owner::Builtin)
                    .is_ok()
            );
        }
        for bad in ["", "Read", "9a", "_a", "a-b", "a.b", &"a".repeat(65)] {
            let err = descriptor(bad, object.clone())
                .validate(&Owner::Builtin)
                .unwrap_err();
            assert_eq!(err.code().code, "tool.invalid_registration");
            assert_eq!(
                err.message(),
                format!("tool '{bad}': name must match ^[a-z][a-z0-9_]{{0,63}}$")
            );
        }
        let mcp = Owner::Mcp("gh".into());
        assert!(
            descriptor("mcp__gh__createIssue", object)
                .validate(&mcp)
                .is_ok()
        );
    }

    #[test]
    fn schema_root_must_be_an_object() {
        for bad in [json!([]), json!("object"), json!({"type": "array"})] {
            let err = descriptor("lookup", bad)
                .validate(&Owner::Builtin)
                .unwrap_err();
            assert_eq!(
                err.message(),
                "tool 'lookup': parameters schema root must be an object"
            );
        }
        assert!(
            descriptor("lookup", json!({"properties": {}}))
                .validate(&Owner::Builtin)
                .is_ok()
        );
    }
}

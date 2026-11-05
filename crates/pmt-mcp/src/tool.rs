//! An MCP server's tools in the tool registry (spec: tool-system, MCP tool
//! naming and filtering; MCP runtime safety; MCP resource and prompt
//! utility tools; plugin-system, MCP tool costs and spend limits).
//!
//! Each admitted tool registers as `mcp__<server>__<tool>` in toolset
//! `mcp-<server>`, owned by the server, with its description stripped of
//! TAG characters. A tool whose `readOnlyHint` is true has no effect (and
//! may run beside other calls); any other tool of an `untrusted` server
//! asks for approval on every call. A costed tool asks through a per-call
//! guard instead, whose prompt states the cost and the conversation's
//! running total, and which asks once-only approval whatever the trust
//! when the call would cross the spend limit. A call goes to the server
//! under its own name and the server's call timeout; its result is
//! converted by [`crate::result`].

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::McpTrust;
use pmt_tools::descriptor::{
    CallContext, CallGuard, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims,
    ToolDescriptor, ToolHandler,
};
use pmt_tools::dispatch::Escalation;
use pmt_tools::probe::{Probe, ProbeContext, ProbeFuture};
use pmt_tools::{Status, ToolResult};
use rmcp::model::Tool;
use serde_json::Value;

use crate::naming;
use crate::result::converted;
use crate::server::Server;

/// Slack between the server's own call timeout and the dispatcher's, so a
/// slow server is reported (and counted by the circuit) by the server.
pub(crate) const TIMEOUT_SLACK: Duration = Duration::from_secs(5);

/// The registry entries of `server`'s admitted tools and utility tools.
#[must_use]
pub fn registered(server: &Arc<Server>) -> Vec<RegisteredTool> {
    let probe: Arc<dyn Probe> = Arc::new(Availability {
        id: format!("mcp:{}", server.name()),
        server: Arc::clone(server),
    });
    let tools = server.tools();
    let mut entries: Vec<RegisteredTool> = tools
        .iter()
        .filter(|tool| server.settings().filter.admits(&tool.name))
        .map(|tool| {
            let handler = Arc::new(McpTool {
                server: Arc::clone(server),
                name: tool.name.to_string(),
            });
            let entry = RegisteredTool::new(
                descriptor(server, tool),
                Owner::Mcp(server.name().to_owned()),
                handler,
            );
            match guard(server, tool) {
                Some(guard) => entry.with_guard(guard),
                None => entry,
            }
        })
        .collect();
    let offered: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    entries.extend(crate::utility::registered(server, &offered));
    entries
        .into_iter()
        .map(|entry| entry.with_probe(Arc::clone(&probe)))
        .collect()
}

fn read_only(tool: &Tool) -> bool {
    tool.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
}

fn descriptor(server: &Server, tool: &Tool) -> ToolDescriptor {
    let name = naming::tool_name(server.name(), &tool.name);
    let description = tool
        .description
        .as_deref()
        .map(str::to_owned)
        .or_else(|| tool.title.clone())
        .unwrap_or_else(|| tool.name.to_string());
    let parameters =
        crate::sanitize::value_without_tags(&Value::Object((*tool.input_schema).clone()));
    let mut descriptor = ToolDescriptor::new(
        &name,
        naming::toolset(server.name()),
        crate::sanitize::strip_tags(&description),
        parameters,
    );
    let read_only = read_only(tool);
    let hints = tool.annotations.as_ref();
    descriptor.idempotent = read_only || hints.and_then(|a| a.idempotent_hint) == Some(true);
    if read_only {
        descriptor.effect = Effect::None;
        descriptor.claims = Some(ResourceClaims::none());
    }
    descriptor.timeout = Some(server.settings().call_timeout + TIMEOUT_SLACK);
    let costed = server.settings().costs.of(&tool.name).is_some();
    if needs_approval(server, tool) && !costed {
        descriptor.approval = Some(Escalation::new(
            rule_key(server, &tool.name),
            untrusted_reason(server, &tool.name),
        ));
    }
    descriptor
}

fn needs_approval(server: &Server, tool: &Tool) -> bool {
    server.settings().trust == McpTrust::Untrusted && !read_only(tool)
}

fn rule_key(server: &Server, tool: &str) -> String {
    format!("mcp:{}:{tool}", server.name())
}

fn untrusted_reason(server: &Server, tool: &str) -> String {
    format!(
        "MCP server '{}' is untrusted and {tool} does not declare itself read-only",
        server.name()
    )
}

/// The per-call approval rule of a costed tool.
fn guard(server: &Arc<Server>, tool: &Tool) -> Option<CallGuard> {
    server.settings().costs.of(&tool.name)?;
    let server = Arc::clone(server);
    let (name, ask) = (tool.name.to_string(), needs_approval(&server, tool));
    Some(Arc::new(move |session: &str, _: &Value| {
        let costs = &server.settings().costs;
        let cost = costs.of(&name)?;
        let spent = server.host().spent(session, server.name());
        let stated = format!(
            "{name} costs {}; {}",
            costs.describe(cost),
            costs.total_line(spent)
        );
        if costs.crosses(spent, cost) {
            return Some(Escalation {
                rule_key: format!("{}:spend", rule_key(&server, &name)),
                reason: format!("{stated}; this call would cross the spend limit"),
                once_only: true,
            });
        }
        ask.then(|| {
            Escalation::new(
                rule_key(&server, &name),
                format!("{}; {stated}", untrusted_reason(&server, &name)),
            )
        })
    }))
}

/// Whether a server's tools may be offered to a new session.
struct Availability {
    id: String,
    server: Arc<Server>,
}

impl Probe for Availability {
    fn id(&self) -> &str {
        &self.id
    }

    fn check<'a>(&'a self, _: ProbeContext<'a>) -> ProbeFuture<'a> {
        let verdict = self.server.availability();
        Box::pin(async move { verdict })
    }
}

/// One tool of one server.
struct McpTool {
    server: Arc<Server>,
    name: String,
}

impl ToolHandler for McpTool {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let arguments = match arguments {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            let session = &context.session_id;
            Ok(
                match self.server.call(session, &self.name, arguments).await {
                    Ok(result) => {
                        let converted = converted(&result, self.server.host().as_ref(), session);
                        self.charge(session, &converted);
                        converted
                    }
                    Err(message) => ToolResult::error(message),
                },
            )
        })
    }
}

impl McpTool {
    /// Adds a costed call that ran to the conversation's total.
    fn charge(&self, session: &str, result: &ToolResult) {
        if result.status == Status::Error {
            return;
        }
        if let Some(cost) = self.server.settings().costs.of(&self.name) {
            self.server
                .host()
                .add_spent(session, self.server.name(), cost.high);
        }
    }
}

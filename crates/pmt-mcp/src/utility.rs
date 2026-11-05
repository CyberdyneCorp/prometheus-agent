//! Resource and prompt utility tools (spec: tool-system, MCP resource and
//! prompt utility tools).
//!
//! A server that advertised `resources` gets `mcp__<server>__list_resources`
//! and `mcp__<server>__read_resource`; one that advertised `prompts` gets
//! `mcp__<server>__list_prompts` and `mcp__<server>__get_prompt`. They live
//! in toolset `mcp-<server>` under the server's include/exclude filter
//! (matched against `list_resources` etc.), never shadow a tool of the
//! server with the same name, and do not exist for a server that did not
//! advertise the capability. Their answers are sanitized like results.

use std::sync::Arc;

use pmt_tools::ToolResult;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use serde_json::{Value, json};

use crate::naming;
use crate::sanitize::value_without_tags;
use crate::server::Server;

/// The four utility tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Utility {
    /// `resources/list`.
    ListResources,
    /// `resources/read`.
    ReadResource,
    /// `prompts/list`.
    ListPrompts,
    /// `prompts/get`.
    GetPrompt,
}

impl Utility {
    /// The tool's own name (before `mcp__<server>__`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::ListResources => "list_resources",
            Self::ReadResource => "read_resource",
            Self::ListPrompts => "list_prompts",
            Self::GetPrompt => "get_prompt",
        }
    }

    fn description(self, server: &str) -> String {
        match self {
            Self::ListResources => format!("List the resources MCP server '{server}' offers."),
            Self::ReadResource => format!("Read one resource of MCP server '{server}' by its URI."),
            Self::ListPrompts => format!("List the prompts MCP server '{server}' offers."),
            Self::GetPrompt => {
                format!("Get one prompt of MCP server '{server}' with its arguments.")
            }
        }
    }

    fn parameters(self) -> Value {
        match self {
            Self::ListResources | Self::ListPrompts => {
                json!({"type": "object", "properties": {}, "additionalProperties": false})
            }
            Self::ReadResource => json!({
                "type": "object",
                "properties": {"uri": {"type": "string", "description": "The resource URI."}},
                "required": ["uri"],
                "additionalProperties": false,
            }),
            Self::GetPrompt => json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The prompt's name."},
                    "arguments": {
                        "type": "object",
                        "description": "The prompt's arguments (strings).",
                        "additionalProperties": {"type": "string"},
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            }),
        }
    }
}

/// The utility tools of `server` (whose own tools are named `offered`).
#[must_use]
pub fn registered(server: &Arc<Server>, offered: &[&str]) -> Vec<RegisteredTool> {
    let offers = server.offers();
    let mut utilities = Vec::new();
    if offers.resources {
        utilities.extend([Utility::ListResources, Utility::ReadResource]);
    }
    if offers.prompts {
        utilities.extend([Utility::ListPrompts, Utility::GetPrompt]);
    }
    utilities
        .into_iter()
        .filter(|u| server.settings().filter.admits(u.name()) && !offered.contains(&u.name()))
        .map(|utility| {
            let mut descriptor = ToolDescriptor::new(
                naming::tool_name(server.name(), utility.name()),
                naming::toolset(server.name()),
                utility.description(server.name()),
                utility.parameters(),
            );
            descriptor.effect = Effect::None;
            descriptor.idempotent = true;
            descriptor.claims = Some(ResourceClaims::none());
            descriptor.timeout = Some(server.settings().call_timeout + crate::tool::TIMEOUT_SLACK);
            RegisteredTool::new(
                descriptor,
                Owner::Mcp(server.name().to_owned()),
                Arc::new(UtilityTool {
                    server: Arc::clone(server),
                    utility,
                }),
            )
        })
        .collect()
}

struct UtilityTool {
    server: Arc<Server>,
    utility: Utility,
}

impl ToolHandler for UtilityTool {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let server = Arc::clone(&self.server);
            let (utility, session) = (self.utility, context.session_id.clone());
            let answer =
                crate::runtime::run(
                    async move { run(&server, utility, &session, &arguments).await },
                )
                .await;
            Ok(match answer {
                Ok(value) => ToolResult::ok(value_without_tags(&value).to_string()),
                Err(message) => ToolResult::error(message),
            })
        })
    }
}

async fn run(
    server: &Arc<Server>,
    utility: Utility,
    session: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let timeout = server.settings().call_timeout;
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let what = utility.name();
    match utility {
        Utility::ListResources => {
            server
                .with_connection(
                    session,
                    what,
                    |c| async move { c.list_resources(timeout).await },
                )
                .await
        }
        Utility::ReadResource => {
            let uri = text("uri");
            server
                .with_connection(session, what, |c| async move {
                    c.read_resource(&uri, timeout).await
                })
                .await
        }
        Utility::ListPrompts => {
            server
                .with_connection(
                    session,
                    what,
                    |c| async move { c.list_prompts(timeout).await },
                )
                .await
        }
        Utility::GetPrompt => {
            let name = text("name");
            let args = arguments
                .get("arguments")
                .and_then(Value::as_object)
                .cloned();
            server
                .with_connection(session, what, |c| async move {
                    c.get_prompt(&name, args, timeout).await
                })
                .await
        }
    }
}

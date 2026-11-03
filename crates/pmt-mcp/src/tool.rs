//! An MCP server's tools in the tool registry (spec: tool-system, MCP tool
//! naming and filtering; MCP runtime safety; Typed tool result contract).
//!
//! Each admitted tool registers as `mcp__<server>__<tool>` in toolset
//! `mcp-<server>`, owned by the server. A tool whose `readOnlyHint` is true
//! has no effect (and may run beside other calls); any other tool of an
//! `untrusted` server asks for approval on every call. A call goes to the
//! server under its own name and the server's call timeout; the result's
//! text, embedded text resources and structured content become the tool
//! result, and media parts are described in text.

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::McpTrust;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use pmt_tools::dispatch::Escalation;
use pmt_tools::probe::{Probe, ProbeContext, ProbeFuture};
use pmt_tools::{Status, ToolResult};
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents, Tool};
use serde_json::Value;

use crate::naming;
use crate::server::Server;

/// Slack between the server's own call timeout and the dispatcher's, so a
/// slow server is reported (and counted by the circuit) by the server.
const TIMEOUT_SLACK: Duration = Duration::from_secs(5);

/// The registry entries of `server`'s admitted tools.
#[must_use]
pub fn registered(server: &Arc<Server>) -> Vec<RegisteredTool> {
    let probe: Arc<dyn Probe> = Arc::new(Availability {
        id: format!("mcp:{}", server.name()),
        server: Arc::clone(server),
    });
    server
        .tools()
        .into_iter()
        .filter(|tool| server.settings().filter.admits(&tool.name))
        .map(|tool| {
            let handler = Arc::new(McpTool {
                server: Arc::clone(server),
                name: tool.name.to_string(),
            });
            RegisteredTool::new(
                descriptor(server, &tool),
                Owner::Mcp(server.name().to_owned()),
                handler,
            )
            .with_probe(Arc::clone(&probe))
        })
        .collect()
}

fn descriptor(server: &Server, tool: &Tool) -> ToolDescriptor {
    let name = naming::tool_name(server.name(), &tool.name);
    let description = tool
        .description
        .as_deref()
        .map(str::to_owned)
        .or_else(|| tool.title.clone())
        .unwrap_or_else(|| tool.name.to_string());
    let parameters = Value::Object((*tool.input_schema).clone());
    let mut descriptor = ToolDescriptor::new(
        &name,
        naming::toolset(server.name()),
        description,
        parameters,
    );
    let hints = tool.annotations.as_ref();
    let read_only = hints.and_then(|a| a.read_only_hint) == Some(true);
    descriptor.idempotent = read_only || hints.and_then(|a| a.idempotent_hint) == Some(true);
    if read_only {
        descriptor.effect = Effect::None;
        descriptor.claims = Some(ResourceClaims::none());
    }
    descriptor.timeout = Some(server.settings().call_timeout + TIMEOUT_SLACK);
    if server.settings().trust == McpTrust::Untrusted && !read_only {
        descriptor.approval = Some(Escalation {
            rule_key: format!("mcp:{}:{}", server.name(), tool.name),
            reason: format!(
                "MCP server '{}' is untrusted and {} does not declare itself read-only",
                server.name(),
                tool.name
            ),
        });
    }
    descriptor
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
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let arguments = match arguments {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            Ok(match self.server.call(&self.name, arguments).await {
                Ok(result) => converted(&result),
                Err(message) => ToolResult::error(message),
            })
        })
    }
}

/// The tool result of an MCP call result.
#[must_use]
pub fn converted(result: &CallToolResult) -> ToolResult {
    let mut parts: Vec<String> = result.content.iter().map(text_of).collect();
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(structured.to_string());
    }
    let status = if result.is_error == Some(true) {
        Status::Error
    } else {
        Status::Ok
    };
    ToolResult::text(status, parts.join("\n"))
}

fn text_of(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => media("image", &image.mime_type, image.data.len()),
        ContentBlock::Audio(audio) => media("audio", &audio.mime_type, audio.data.len()),
        ContentBlock::Resource(embedded) => match &embedded.resource {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => media(
                &format!("resource {uri}"),
                mime_type.as_deref().unwrap_or("application/octet-stream"),
                blob.len(),
            ),
            _ => "[resource]".to_owned(),
        },
        ContentBlock::ResourceLink(link) => format!("[resource link: {}]", link.uri),
        _ => "[content]".to_owned(),
    }
}

/// A media part in words (`[image image/png, about 12 KiB]`).
fn media(kind: &str, mime: &str, base64_len: usize) -> String {
    let bytes = base64_len / 4 * 3;
    format!("[{kind} {mime}, about {} KiB]", bytes.div_ceil(1024))
}

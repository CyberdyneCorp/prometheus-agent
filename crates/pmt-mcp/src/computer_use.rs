//! The computer-use driver over MCP stdio (spec: computer-use, Driver
//! runtime management and identity): `cua-driver mcp` is an MCP server;
//! [`McpDriver`] keeps one connection to it as a
//! [`pmt_tools::computer_use::Driver`]. The child process dies with the
//! connection, so dropping the driver stops the runtime.

use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use pmt_tools::computer_use::{Driver, DriverReply};
use pmt_tools::dispatch::SeamFuture;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::connection::{CallError, Connection, Launch, span};
use crate::handler::Handler;

/// How long one driver call may take (an accessibility walk of a large
/// window can take seconds).
pub const CALL_TIMEOUT: Duration = Duration::from_mins(1);

/// One live driver connection.
pub struct McpDriver {
    connection: Arc<Connection>,
    tools: Vec<String>,
    /// Kept alive with the connection (a private runtime's daemon).
    _owned: Box<dyn Send + Sync>,
}

impl McpDriver {
    /// Starts `command` (the driver's MCP server) and completes the
    /// handshake within `timeout`; `owned` lives as long as the driver
    /// (a private runtime's daemon guard).
    ///
    /// # Errors
    ///
    /// Why the driver did not start or answer.
    pub async fn open(
        command: Command,
        timeout: Duration,
        owned: Box<dyn Send + Sync>,
    ) -> Result<Self, String> {
        let handler = Handler::new(Weak::new(), false, false);
        let connection = crate::runtime::run(async move {
            Connection::open(Launch::Stdio(command), handler, timeout).await
        })
        .await?;
        let tools = connection
            .tools()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        Ok(Self {
            connection: Arc::new(connection),
            tools,
            _owned: owned,
        })
    }
}

impl Driver for McpDriver {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Map<String, Value>,
    ) -> SeamFuture<'a, Result<DriverReply, String>> {
        let connection = Arc::clone(&self.connection);
        let name = tool.to_owned();
        Box::pin(async move {
            let outcome = crate::runtime::run(async move {
                connection.call(&name, arguments, CALL_TIMEOUT).await
            })
            .await;
            match outcome {
                Ok(result) => Ok(reply(&result)),
                Err(CallError::Closed) => {
                    Err(format!("the driver closed the connection during {tool}"))
                }
                Err(CallError::TimedOut(after)) => Err(format!(
                    "{tool} gave no answer within {}; its outcome is unknown, capture again before acting",
                    span(after)
                )),
                Err(CallError::Failed(why)) => Err(why),
            }
        })
    }

    fn tools(&self) -> Vec<String> {
        self.tools.clone()
    }
}

/// An MCP call result as a driver reply.
#[must_use]
pub fn reply(result: &CallToolResult) -> DriverReply {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => texts.push(text.text.clone()),
            ContentBlock::Image(image) => {
                if let Ok(bytes) = STANDARD.decode(image.data.as_bytes()) {
                    images.push((image.mime_type.clone(), bytes));
                }
            }
            _ => {}
        }
    }
    let structured = result.structured_content.clone().unwrap_or_else(|| {
        // Older drivers put their JSON in the text.
        let joined = texts.join("\n");
        serde_json::from_str::<Value>(&joined)
            .ok()
            .filter(Value::is_object)
            .unwrap_or(Value::Null)
    });
    DriverReply {
        is_error: result.is_error == Some(true),
        text: texts.join("\n"),
        structured,
        images,
    }
}

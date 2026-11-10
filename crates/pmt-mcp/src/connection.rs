//! Starting one MCP server: the transport, the `initialize` handshake and
//! the tool list (spec: plugin-system, MCP client servers).
//!
//! A `stdio` server is a child process whose command the caller built (the
//! binary builds it under the execution sandbox with only the server's
//! declared environment); its stdout and stdin carry JSON-RPC and its
//! stderr goes to the log. An `http` server is a streamable HTTP endpoint
//! reached through the caller's HTTP client. Either way the connection is
//! owned by the MCP runtime and the child process dies with it.

use std::process::Stdio;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, GetPromptRequestParams, JsonObject,
    ReadResourceRequestParams, Tool,
};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};

use crate::handler::Handler;

/// How a server is reached.
pub enum Launch {
    /// A child process speaking JSON-RPC on stdin and stdout.
    Stdio(Command),
    /// A streamable HTTP endpoint.
    Http {
        /// The endpoint.
        url: String,
        /// The client (the profile's outbound policy).
        client: reqwest::Client,
        /// Sent as `Authorization: Bearer <token>`.
        bearer: Option<String>,
        /// Sent with every request (`headers`, the identity header).
        headers: Vec<(String, String)>,
    },
}

impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio(command) => f
                .debug_tuple("Stdio")
                .field(&command.as_std().get_program())
                .finish(),
            Self::Http { url, .. } => f
                .debug_struct("Http")
                .field("url", url)
                .finish_non_exhaustive(),
        }
    }
}

/// Why a call did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The connection is gone (the process exited, the endpoint closed).
    Closed,
    /// No answer within the call timeout.
    TimedOut(Duration),
    /// The server answered with a protocol error, or the request failed.
    Failed(String),
}

/// What a server offers besides tools (spec: tool-system, MCP resource
/// and prompt utility tools).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offers {
    /// It advertised `resources`.
    pub resources: bool,
    /// It advertised `prompts`.
    pub prompts: bool,
}

/// One live connection.
pub struct Connection {
    service: RunningService<RoleClient, Handler>,
    tools: Vec<Tool>,
    offers: Offers,
    /// The server's process; killed when the connection is dropped.
    _child: Option<Child>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("tools", &self.tools.len())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Starts the server and lists its tools, all within `timeout`. Must
    /// run on the MCP runtime ([`crate::runtime::run`]).
    ///
    /// # Errors
    ///
    /// Why the server could not be started or did not complete the
    /// handshake or the tool listing in time.
    pub async fn open(launch: Launch, handler: Handler, timeout: Duration) -> Result<Self, String> {
        match tokio::time::timeout(timeout, Self::open_now(launch, handler)).await {
            Ok(result) => result,
            Err(_) => Err(format!("no answer to initialize within {}", span(timeout))),
        }
    }

    async fn open_now(launch: Launch, handler: Handler) -> Result<Self, String> {
        let (service, child) = match launch {
            Launch::Stdio(command) => {
                let (service, child) = stdio(command, handler).await?;
                (service, Some(child))
            }
            Launch::Http {
                url,
                client,
                bearer,
                headers,
            } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url)
                    .custom_headers(header_map(&headers)?);
                if let Some(token) = bearer {
                    config = config.auth_header(token);
                }
                let transport = StreamableHttpClientTransport::with_client(client, config);
                let service = handler.serve(transport).await.map_err(|e| e.to_string())?;
                (service, None)
            }
        };
        let offers = service
            .peer_info()
            .map(|info| Offers {
                resources: info.capabilities.resources.is_some(),
                prompts: info.capabilities.prompts.is_some(),
            })
            .unwrap_or_default();
        let tools = service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        Ok(Self {
            service,
            tools,
            offers,
            _child: child,
        })
    }

    /// What it offers besides tools.
    #[must_use]
    pub fn offers(&self) -> Offers {
        self.offers
    }

    /// Lists the server's tools again (after `tools/list_changed`).
    ///
    /// # Errors
    ///
    /// The request failed.
    pub async fn list_tools(&self, timeout: Duration) -> Result<Vec<Tool>, CallError> {
        self.bounded(timeout, self.service.peer().list_all_tools())
            .await
    }

    /// `resources/list`, as JSON.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub async fn list_resources(&self, timeout: Duration) -> Result<Value, CallError> {
        let resources = self
            .bounded(timeout, self.service.peer().list_all_resources())
            .await?;
        Ok(serde_json::to_value(resources).unwrap_or_default())
    }

    /// `resources/read` of `uri`, as JSON.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub async fn read_resource(&self, uri: &str, timeout: Duration) -> Result<Value, CallError> {
        let params = ReadResourceRequestParams::new(uri.to_owned());
        let read = self
            .bounded(timeout, self.service.read_resource(params))
            .await?;
        Ok(serde_json::to_value(read).unwrap_or_default())
    }

    /// `prompts/list`, as JSON.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub async fn list_prompts(&self, timeout: Duration) -> Result<Value, CallError> {
        let prompts = self
            .bounded(timeout, self.service.peer().list_all_prompts())
            .await?;
        Ok(serde_json::to_value(prompts).unwrap_or_default())
    }

    /// `prompts/get` of `name`, as JSON.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let mut params = GetPromptRequestParams::new(name.to_owned());
        params.arguments = arguments;
        let prompt = self
            .bounded(timeout, self.service.get_prompt(params))
            .await?;
        Ok(serde_json::to_value(prompt).unwrap_or_default())
    }

    async fn bounded<T>(
        &self,
        timeout: Duration,
        request: impl Future<Output = Result<T, ServiceError>>,
    ) -> Result<T, CallError> {
        match tokio::time::timeout(timeout, request).await {
            Err(_) => Err(CallError::TimedOut(timeout)),
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(self.classify(err)),
        }
    }

    fn classify(&self, err: ServiceError) -> CallError {
        match err {
            ServiceError::TransportClosed | ServiceError::TransportSend(_) => CallError::Closed,
            _ if self.is_closed() => CallError::Closed,
            err => CallError::Failed(err.to_string()),
        }
    }

    /// The server's tools as it listed them at connect.
    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Whether the connection is gone.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.service.is_closed()
    }

    /// Calls the server's tool `name`, within `timeout`.
    ///
    /// # Errors
    ///
    /// [`CallError`].
    pub async fn call(
        &self,
        name: &str,
        arguments: JsonObject,
        timeout: Duration,
    ) -> Result<CallToolResult, CallError> {
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        self.bounded(timeout, self.service.call_tool(params)).await
    }
}

/// Spawns `command` with piped stdio and serves the client over its
/// pipes; its stderr lines go to the log at DEBUG.
async fn stdio(
    mut command: Command,
    handler: Handler,
) -> Result<(RunningService<RoleClient, Handler>, Child), String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start {program}: {e}"))?;
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        return Err(format!("{program}: no stdio pipes"));
    };
    if let Some(stderr) = child.stderr.take() {
        pmt_core::spawn::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "mcp", program = %program, "{line}");
            }
        });
    }
    match handler.serve((stdout, stdin)).await {
        Ok(service) => Ok((service, child)),
        Err(err) => {
            // `serve` fails on stdout EOF, which can come before the
            // crashed child is reaped: give it a moment to report its exit.
            let status = tokio::time::timeout(EXIT_GRACE, child.wait())
                .await
                .ok()
                .and_then(Result::ok);
            Err(match status {
                Some(status) => format!("the server process exited ({status})"),
                None => format!("initialize failed: {err}"),
            })
        }
    }
}

/// How long a server that failed the handshake gets to exit, so a crash
/// is reported as one rather than as a protocol error.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// `headers` as a header map.
fn header_map(
    headers: &[(String, String)],
) -> Result<
    std::collections::HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    String,
> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| format!("header name '{name}': {e}"))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| format!("header '{name}': {e}"))?;
            Ok((name, value))
        })
        .collect()
}

/// `250 ms`, `60 s`.
#[must_use]
pub fn span(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{} s", duration.as_secs())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Weak;

    use tokio::process::Command;

    use super::stdio;
    use crate::handler::Handler;

    /// A server that closes stdout and exits a moment later is reported
    /// as an exited process, not as a protocol error: the handshake sees
    /// EOF before the child has been reaped.
    #[tokio::test]
    async fn a_server_crashing_after_closing_stdout_is_reported_as_exited() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec 1>&-; sleep 0.2; exit 4"]);
        let Err(message) = stdio(command, Handler::new(Weak::new(), false, false)).await else {
            panic!("the handshake cannot succeed");
        };
        assert!(
            message.starts_with("the server process exited"),
            "{message}"
        );
    }
}

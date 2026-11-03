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
use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject, Tool};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};

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

/// One live connection.
pub struct Connection {
    service: RunningService<RoleClient, ()>,
    tools: Vec<Tool>,
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
    pub async fn open(launch: Launch, timeout: Duration) -> Result<Self, String> {
        match tokio::time::timeout(timeout, Self::open_now(launch)).await {
            Ok(result) => result,
            Err(_) => Err(format!("no answer to initialize within {}", span(timeout))),
        }
    }

    async fn open_now(launch: Launch) -> Result<Self, String> {
        let (service, child) = match launch {
            Launch::Stdio(command) => {
                let (service, child) = stdio(command).await?;
                (service, Some(child))
            }
            Launch::Http {
                url,
                client,
                bearer,
            } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url);
                if let Some(token) = bearer {
                    config = config.auth_header(token);
                }
                let transport = StreamableHttpClientTransport::with_client(client, config);
                let service = ().serve(transport).await.map_err(|e| e.to_string())?;
                (service, None)
            }
        };
        let tools = service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        Ok(Self {
            service,
            tools,
            _child: child,
        })
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
        match tokio::time::timeout(timeout, self.service.call_tool(params)).await {
            Err(_) => Err(CallError::TimedOut(timeout)),
            Ok(Ok(result)) => Ok(result),
            Ok(Err(ServiceError::TransportClosed | ServiceError::TransportSend(_))) => {
                Err(CallError::Closed)
            }
            Ok(Err(_)) if self.is_closed() => Err(CallError::Closed),
            Ok(Err(err)) => Err(CallError::Failed(err.to_string())),
        }
    }
}

/// Spawns `command` with piped stdio and serves the client over its
/// pipes; its stderr lines go to the log at DEBUG.
async fn stdio(mut command: Command) -> Result<(RunningService<RoleClient, ()>, Child), String> {
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
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "mcp", program = %program, "{line}");
            }
        });
    }
    match ().serve((stdout, stdin)).await {
        Ok(service) => Ok((service, child)),
        Err(err) => {
            let status = child.try_wait().ok().flatten();
            Err(match status {
                Some(status) => format!("the server process exited ({status})"),
                None => format!("initialize failed: {err}"),
            })
        }
    }
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

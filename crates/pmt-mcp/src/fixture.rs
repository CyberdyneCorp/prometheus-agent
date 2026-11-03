//! The reference (and, on request, misbehaving) MCP server of the tests
//! (feature `fixture`; the `pmt-mcp-fixture` binary serves it).
//!
//! Tools: `echo {text}` (read-only), `add {a, b}`, `create_issue {title}`,
//! `delete_issue {id}`, `env` (read-only: the names of its environment
//! variables), `fail` (an error result), `sleep {ms}`, `crash` (the
//! process exits) and `do.thing-x` (a name that needs sanitizing). Every
//! call is appended to the log file given at start, when there is one.

use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use serde_json::{Map, Value, json};

/// The fixture server.
#[derive(Debug, Clone, Default)]
pub struct Fixture {
    /// Where each call is recorded (`call <tool>` per line).
    pub log: Option<PathBuf>,
}

fn tool(name: &str, description: &str, schema: Value, read_only: bool) -> Tool {
    let Value::Object(schema) = schema else {
        unreachable!("fixture schemas are objects")
    };
    let mut tool = Tool::new(name.to_owned(), description.to_owned(), Arc::new(schema));
    if read_only {
        tool.annotations = Some(ToolAnnotations::new().read_only(true));
    }
    tool
}

/// The fixture's tools.
#[must_use]
pub fn tools() -> Vec<Tool> {
    let object = |properties: Value, required: &[&str]| json!({"type": "object", "properties": properties, "required": required});
    vec![
        tool(
            "echo",
            "Echo the text back.",
            object(json!({"text": {"type": "string"}}), &["text"]),
            true,
        ),
        tool(
            "add",
            "Add two numbers.",
            object(
                json!({"a": {"type": "number"}, "b": {"type": "number"}}),
                &["a", "b"],
            ),
            false,
        ),
        tool(
            "create_issue",
            "Create an issue.",
            object(json!({"title": {"type": "string"}}), &["title"]),
            false,
        ),
        tool(
            "delete_issue",
            "Delete an issue.",
            object(json!({"id": {"type": "integer"}}), &["id"]),
            false,
        ),
        tool(
            "env",
            "List the server's environment variable names.",
            json!({"type": "object"}),
            true,
        ),
        tool("fail", "Always fails.", json!({"type": "object"}), true),
        tool(
            "sleep",
            "Sleep for ms milliseconds.",
            object(json!({"ms": {"type": "integer"}}), &["ms"]),
            true,
        ),
        tool(
            "crash",
            "Exit the server process.",
            json!({"type": "object"}),
            false,
        ),
        tool(
            "do.thing-x",
            "A tool whose name needs sanitizing.",
            json!({"type": "object"}),
            true,
        ),
    ]
}

impl Fixture {
    fn record(&self, name: &str) {
        let Some(path) = &self.log else { return };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "call {name}");
        }
    }

    async fn run(
        &self,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<CallToolResult, ErrorData> {
        let text = |t: String| Ok(CallToolResult::success(vec![ContentBlock::text(t)]));
        match name {
            "echo" => text(
                args.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            ),
            "add" => {
                let n = |k: &str| args.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                text(format!("{}", n("a") + n("b")))
            }
            "create_issue" => text(format!(
                "created {}",
                args.get("title").and_then(Value::as_str).unwrap_or("")
            )),
            "delete_issue" => text("deleted".to_owned()),
            "env" => {
                let mut names: Vec<String> = std::env::vars_os()
                    .map(|(k, _)| k.to_string_lossy().into_owned())
                    .collect();
                names.sort();
                text(names.join("\n"))
            }
            "fail" => Ok(CallToolResult::error(vec![ContentBlock::text("it failed")])),
            "sleep" => {
                let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                text("slept".to_owned())
            }
            "crash" => std::process::exit(3),
            "do.thing-x" => text("done".to_owned()),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tools())))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.record(&request.name);
        let args = request.arguments.unwrap_or_default();
        self.run(&request.name, &args)
            .await
            .map(CallToolResponse::Complete)
    }
}

/// Serves the fixture over streamable HTTP on `listener` (no
/// authentication; `bearer` makes it require that token like a real
/// server would).
///
/// # Errors
///
/// The listener failed.
pub async fn serve_http(
    fixture: Fixture,
    listener: tokio::net::TcpListener,
    bearer: Option<String>,
) -> Result<(), String> {
    use http_body_util::BodyExt as _;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

    let service = StreamableHttpService::new(
        move || Ok(fixture.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    loop {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let (service, bearer) = (service.clone(), bearer.clone());
        tokio::spawn(async move {
            let handler = hyper::service::service_fn(
                move |request: hyper::Request<hyper::body::Incoming>| {
                    let (service, bearer) = (service.clone(), bearer.clone());
                    async move {
                        let wanted = bearer.map(|t| format!("Bearer {t}"));
                        let given = request
                            .headers()
                            .get(hyper::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        if wanted.is_some() && wanted != given {
                            let body = http_body_util::Full::new(bytes::Bytes::new()).boxed();
                            let mut response = hyper::Response::new(body);
                            *response.status_mut() = hyper::StatusCode::UNAUTHORIZED;
                            return Ok::<_, std::convert::Infallible>(response);
                        }
                        Ok(service.handle(request).await)
                    }
                },
            );
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), handler)
                .await;
        });
    }
}

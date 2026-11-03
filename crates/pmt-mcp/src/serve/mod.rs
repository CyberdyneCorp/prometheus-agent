//! MCP server mode: Prometheus tools and skills offered to MCP clients
//! (spec: plugin-system, MCP server mode).
//!
//! What is offered comes from a [`Host`] the binary builds: the exposed
//! tools (each call runs through the session dispatcher, so the profile's
//! approval policy and sandbox apply), and the skills, which are offered
//! both as prompts and as `skill://<name>` resources. Nothing else is
//! offered. The transports are stdio and streamable HTTP ([`http`]).

pub mod http;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pmt_tools::ToolResult;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, Prompt, PromptMessage, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, Role, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{MaybeSendFuture, RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt as _};
use serde_json::Value;

/// The resource URI scheme of skills.
pub const SKILL_SCHEME: &str = "skill://";

/// A boxed future.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One exposed tool.
#[derive(Debug, Clone)]
pub struct ExposedTool {
    /// Its name.
    pub name: String,
    /// Its description.
    pub description: String,
    /// Its argument schema (an object schema).
    pub parameters: Value,
    /// Whether it only reads.
    pub read_only: bool,
}

/// One skill.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// Its name.
    pub name: String,
    /// Its one-line description.
    pub description: String,
}

/// What the server offers.
pub trait Host: Send + Sync + 'static {
    /// The exposed tools.
    fn tools(&self) -> Vec<ExposedTool>;
    /// Runs exposed tool `name` through the dispatch pipeline.
    fn call(&self, name: String, arguments: Value) -> HostFuture<'_, ToolResult>;
    /// The skills.
    fn skills(&self) -> Vec<SkillEntry>;
    /// The full text of skill `name`.
    fn skill(&self, name: String) -> HostFuture<'_, Result<String, String>>;
}

/// The MCP server over a [`Host`].
#[derive(Clone)]
pub struct Serving {
    host: Arc<dyn Host>,
}

impl Serving {
    /// Serves `host`.
    #[must_use]
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host }
    }

    async fn skill_text(&self, name: &str) -> Result<String, ErrorData> {
        if !self.host.skills().iter().any(|s| s.name == name) {
            return Err(ErrorData::invalid_params(
                format!("unknown skill: {name}"),
                None,
            ));
        }
        self.host
            .skill(name.to_owned())
            .await
            .map_err(|e| ErrorData::internal_error(e, None))
    }
}

/// Serves `host` on this process's stdin and stdout until the client
/// disconnects.
///
/// # Errors
///
/// The handshake failed.
pub async fn stdio(host: Arc<dyn Host>) -> Result<(), String> {
    let running = Serving::new(host)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| e.to_string())?;
    running
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

impl ServerHandler for Serving {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(Implementation::new("prometheus", env!("CARGO_PKG_VERSION")))
    }

    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + MaybeSendFuture + '_ {
        let tools = self.host.tools().into_iter().map(mcp_tool).collect();
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        if !self.host.tools().iter().any(|t| t.name == name) {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = self.host.call(name, arguments).await;
        Ok(CallToolResponse::Complete(call_result(&result)))
    }

    fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, ErrorData>> + MaybeSendFuture + '_ {
        let prompts = self
            .host
            .skills()
            .into_iter()
            .map(|s| Prompt::new(s.name, Some(s.description), None))
            .collect();
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts)))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        let text = self.skill_text(&request.name).await?;
        let message = PromptMessage::new_text(Role::User, text);
        Ok(GetPromptResponse::Complete(
            GetPromptResult::new(vec![message]).with_description(format!("skill {}", request.name)),
        ))
    }

    fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, ErrorData>> + MaybeSendFuture + '_ {
        let resources = self
            .host
            .skills()
            .into_iter()
            .map(|s| {
                Resource::new(format!("{SKILL_SCHEME}{}", s.name), s.name)
                    .with_description(s.description)
                    .with_mime_type("text/markdown")
            })
            .collect();
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let Some(name) = request.uri.strip_prefix(SKILL_SCHEME) else {
            return Err(ErrorData::resource_not_found(
                format!("no resource {}", request.uri),
                None,
            ));
        };
        let text = self.skill_text(name).await?;
        let contents = ResourceContents::text(text, request.uri.clone());
        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![contents],
        )))
    }
}

fn mcp_tool(tool: ExposedTool) -> Tool {
    let schema = match tool.parameters {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    let mut mcp = Tool::new(tool.name, tool.description, Arc::new(schema));
    if tool.read_only {
        mcp.annotations = Some(rmcp::model::ToolAnnotations::new().read_only(true));
    }
    mcp
}

/// A tool result as an MCP call result: its text, with `isError` unless
/// the call succeeded.
#[must_use]
pub fn call_result(result: &ToolResult) -> CallToolResult {
    let content = vec![ContentBlock::text(result.text_content())];
    if result.status.succeeded() {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    }
}

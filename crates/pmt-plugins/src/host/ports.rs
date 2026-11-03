//! What the host interfaces reach outside the plugin: the embedder supplies
//! HTTP, secrets, model calls, tool dispatch, the event bus and plugin
//! settings. The host checks the plugin's grant before every port call, so
//! a port never sees an ungranted request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed future returned by the ports.
pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An outbound HTTP request (the host already checked its host against
/// the `network` grant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET`, `POST`...
    pub method: String,
    /// Absolute `http(s)` URL.
    pub url: String,
    /// Header pairs.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Option<Vec<u8>>,
}

/// Its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Header pairs.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

/// Outbound HTTP.
pub trait HttpPort: Send + Sync {
    /// Sends one request.
    fn send(&self, request: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>>;
}

/// Secret values by name.
pub trait SecretsPort: Send + Sync {
    /// The value of `name`, if set.
    fn get(&self, name: &str) -> Option<String>;
}

/// One model call (the host already checked the `llm` grant and any
/// model or provider override).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCall {
    /// The calling plugin (the ledger charges it).
    pub plugin: String,
    /// The session the call runs for, if any.
    pub session_id: Option<String>,
    /// `(role, content)` pairs.
    pub messages: Vec<(String, String)>,
    /// A model override.
    pub model: Option<String>,
    /// A provider override.
    pub provider: Option<String>,
    /// Answer size cap.
    pub max_tokens: Option<u32>,
    /// A JSON Schema the answer must match (`complete-structured`).
    pub schema: Option<String>,
}

/// Model calls on the profile's provider.
pub trait LlmPort: Send + Sync {
    /// Runs one completion and returns its text.
    fn complete(&self, call: LlmCall) -> PortFuture<'_, Result<String, String>>;
}

/// Tool dispatch on behalf of a plugin (`tools_call` grant checked).
pub trait ToolsPort: Send + Sync {
    /// Calls `name` with JSON `arguments` and returns the result text.
    fn dispatch(
        &self,
        session_id: Option<String>,
        name: String,
        arguments: String,
    ) -> PortFuture<'_, Result<String, String>>;
}

/// The plugin event bus.
pub trait EventsPort: Send + Sync {
    /// Publishes `<plugin_id>:<name>` (already namespaced and validated).
    fn emit(&self, profile: &str, name: &str, payload: &str);
    /// Records a subscription of `plugin` to `pattern`.
    fn subscribe(&self, profile: &str, plugin: &str, pattern: &str);
}

/// `plugins.entries.<id>.*` (values are JSON).
pub trait ConfigPort: Send + Sync {
    /// The value of `plugins.entries.<plugin>.<key>`.
    fn get(&self, plugin: &str, key: &str) -> Option<serde_json::Value>;
    /// Writes it.
    ///
    /// # Errors
    ///
    /// Why the value could not be written.
    fn set(&self, plugin: &str, key: &str, value: serde_json::Value) -> Result<(), String>;
}

/// Every port an instance uses.
#[derive(Clone)]
pub struct Ports {
    /// Outbound HTTP.
    pub http: Arc<dyn HttpPort>,
    /// Secret values.
    pub secrets: Arc<dyn SecretsPort>,
    /// Model calls.
    pub llm: Arc<dyn LlmPort>,
    /// Tool dispatch.
    pub tools: Arc<dyn ToolsPort>,
    /// The event bus.
    pub events: Arc<dyn EventsPort>,
    /// Plugin settings.
    pub config: Arc<dyn ConfigPort>,
}

impl Default for Ports {
    /// Ports that answer "not available" (each surface supplies its own).
    fn default() -> Self {
        let none = Arc::new(Unavailable);
        Self {
            http: none.clone(),
            secrets: none.clone(),
            llm: none.clone(),
            tools: none.clone(),
            events: none.clone(),
            config: none,
        }
    }
}

impl std::fmt::Debug for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ports").finish_non_exhaustive()
    }
}

/// A port with nothing behind it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unavailable;

const UNAVAILABLE: &str = "not available on this surface";

impl HttpPort for Unavailable {
    fn send(&self, _: HttpRequest) -> PortFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async { Err(format!("http is {UNAVAILABLE}")) })
    }
}

impl SecretsPort for Unavailable {
    fn get(&self, _: &str) -> Option<String> {
        None
    }
}

impl LlmPort for Unavailable {
    fn complete(&self, _: LlmCall) -> PortFuture<'_, Result<String, String>> {
        Box::pin(async { Err(format!("llm is {UNAVAILABLE}")) })
    }
}

impl ToolsPort for Unavailable {
    fn dispatch(
        &self,
        _: Option<String>,
        _: String,
        _: String,
    ) -> PortFuture<'_, Result<String, String>> {
        Box::pin(async { Err(format!("tool dispatch is {UNAVAILABLE}")) })
    }
}

impl EventsPort for Unavailable {
    fn emit(&self, _: &str, name: &str, _: &str) {
        tracing::debug!(event = name, "plugin event dropped: no event bus");
    }

    fn subscribe(&self, _: &str, _: &str, _: &str) {}
}

impl ConfigPort for Unavailable {
    fn get(&self, _: &str, _: &str) -> Option<serde_json::Value> {
        None
    }

    fn set(&self, _: &str, _: &str, _: serde_json::Value) -> Result<(), String> {
        Err(format!("plugin settings are {UNAVAILABLE}"))
    }
}

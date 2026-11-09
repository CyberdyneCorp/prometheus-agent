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
    /// The plugin sending it (the egress policy names it as the requester).
    pub plugin: String,
    /// The session the call runs for, if any (where a refusal is reported).
    pub session_id: Option<String>,
    /// The `host[:port]` patterns of the plugin's `network` grant other
    /// than `*`: only a host named there may be a private address.
    pub granted_hosts: Vec<String>,
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

/// A credential the host attaches to a plugin's requests (the
/// `credentials` host interface); the plugin never sees `headers`.
#[derive(Clone)]
pub struct Credential {
    /// Which kind it resolved to (`direct`, `managed`).
    pub kind: String,
    /// The base URL requests with it go to.
    pub origin: String,
    /// Who is billed, when not the user's own account.
    pub billing_source: Option<String>,
    /// The hosts it may be sent to (`host` or `host:port`, lower case).
    pub hosts: Vec<String>,
    /// The headers it adds, replacing any of the same name.
    pub headers: Vec<(String, pmt_core::secret::SecretString)>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("kind", &self.kind)
            .field("origin", &self.origin)
            .field("billing_source", &self.billing_source)
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

/// What a credential handle resolves to.
#[derive(Debug, Clone)]
pub enum CredentialLookup {
    /// The host knows no credential by that handle.
    Unknown,
    /// It exists but cannot be used now, with the reason.
    Unavailable(String),
    /// Ready to attach.
    Ready(Credential),
}

/// Credentials held for plugins, by handle (the host already checked the
/// `credentials` grant).
pub trait CredentialsPort: Send + Sync {
    /// The credential `handle` names for `plugin`.
    fn resolve(&self, plugin: &str, handle: &str) -> CredentialLookup;
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

/// A live message the conversation port posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    /// What `edit` takes.
    pub handle: String,
    /// The surface cannot edit, so the text went out as a new message.
    pub fallback: bool,
}

/// Why a live message did not go out (the WIT `conversation.failure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostFailure {
    /// The call is not bound to a session.
    NoConversation,
    /// The session's gateway is not running.
    GatewayUnavailable,
    /// The session's adapter is not connected.
    AdapterUnavailable,
    /// The platform refused the message.
    SendFailed,
    /// The platform refused the edit.
    EditFailed,
    /// No answer within 30 seconds.
    Timeout,
}

/// Where an injected message goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectTarget {
    /// A stored session id.
    SessionKey(String),
    /// A gateway chat, `<platform>:<chat_id>`.
    Origin(String),
}

/// One platform action (`platform.add_reaction` / `set_thread_title`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformAction {
    /// `add_reaction` or `set_thread_title`.
    pub verb: &'static str,
    /// The platform (`discord`, `slack`...).
    pub platform: String,
    /// The chat.
    pub chat_id: String,
    /// The message (`add_reaction`) or thread (`set_thread_title`).
    pub target_id: String,
    /// The emoji or the title.
    pub value: String,
}

/// A refused or failed platform action: one of the codes the WIT
/// `platform.outcome` documents, and detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError {
    /// `gateway_unavailable`, `unknown_platform`, `adapter_not_registered`,
    /// `adapter_disconnected`, `unsupported_platform_action`,
    /// `action_failed`.
    pub code: &'static str,
    /// What happened.
    pub detail: String,
}

/// Messaging services of the instance's own profile: the conversation of
/// a session, the busy-input queue and the connected adapters. The host
/// has checked the grant before every call that needs one.
pub trait MessagingPort: Send + Sync {
    /// Posts `text` to the conversation of `session_id`.
    fn post(&self, session_id: &str, text: &str) -> PortFuture<'_, Result<Posted, PostFailure>>;
    /// Edits a message `post` returned.
    fn edit(
        &self,
        session_id: &str,
        handle: &str,
        text: &str,
    ) -> PortFuture<'_, Result<Posted, PostFailure>>;
    /// Queues `content` as new user input for `target`, a session of this
    /// profile only.
    ///
    /// # Errors
    ///
    /// Why it was refused (logged by the host).
    fn inject(&self, target: &InjectTarget, content: &str) -> PortFuture<'_, Result<(), String>>;
    /// Runs `action` on a connected adapter of this profile.
    fn platform_action(&self, action: &PlatformAction) -> PortFuture<'_, Result<(), ActionError>>;
}

/// One call of the `subagents` interface (the host checked the grant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentRequest {
    /// Start a child; `task` is one `delegate_task` task object as JSON.
    Launch {
        /// The task.
        task: String,
    },
    /// Its state.
    Status {
        /// The handle `launch` returned.
        handle: String,
    },
    /// Wait for its end.
    Wait {
        /// The handle.
        handle: String,
        /// How long at most.
        timeout: std::time::Duration,
    },
    /// Queue a message for its next iteration.
    Steer {
        /// The handle.
        handle: String,
        /// The message.
        message: String,
    },
    /// Ask it to stop.
    Cancel {
        /// The handle.
        handle: String,
    },
    /// Its result once ended.
    Result {
        /// The handle.
        handle: String,
    },
}

/// What a `subagents` call answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentAnswer {
    /// `launch`: the child's handle.
    Handle(String),
    /// `status`: `queued`, `running`, `completed`, `failed`, `interrupted`,
    /// `stalled`, `cancel_requested` or `cancelled`.
    State(&'static str),
    /// `wait` / `result`: the `SubagentResult` JSON, `None` while running.
    Result(Option<String>),
    /// `steer` / `cancel` done.
    Done,
}

/// Why a `subagents` call did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentFailure {
    /// Forged, altered, foreign or expired handle.
    UnknownHandle,
    /// Refused: `<error code>: <message>`.
    Refused(String),
}

/// Subagents of the session a plugin call runs for.
pub trait SubagentsPort: Send + Sync {
    /// Runs `request` for `plugin` in session `session`.
    fn call<'a>(
        &'a self,
        session: &'a str,
        plugin: &'a str,
        request: SubagentRequest,
    ) -> PortFuture<'a, Result<SubagentAnswer, SubagentFailure>>;
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
    /// Live messages, injection and platform actions.
    pub messaging: Arc<dyn MessagingPort>,
    /// Subagents of the calling session.
    pub subagents: Arc<dyn SubagentsPort>,
    /// Credentials attached to plugin requests.
    pub credentials: Arc<dyn CredentialsPort>,
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
            config: none.clone(),
            messaging: none.clone(),
            subagents: none.clone(),
            credentials: none,
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

impl MessagingPort for Unavailable {
    fn post(&self, _: &str, _: &str) -> PortFuture<'_, Result<Posted, PostFailure>> {
        Box::pin(async { Err(PostFailure::GatewayUnavailable) })
    }

    fn edit(&self, _: &str, _: &str, _: &str) -> PortFuture<'_, Result<Posted, PostFailure>> {
        Box::pin(async { Err(PostFailure::GatewayUnavailable) })
    }

    fn inject(&self, _: &InjectTarget, _: &str) -> PortFuture<'_, Result<(), String>> {
        Box::pin(async { Err(format!("message injection is {UNAVAILABLE}")) })
    }

    fn platform_action(&self, _: &PlatformAction) -> PortFuture<'_, Result<(), ActionError>> {
        Box::pin(async {
            Err(ActionError {
                code: "gateway_unavailable",
                detail: format!("platform actions are {UNAVAILABLE}"),
            })
        })
    }
}

impl SubagentsPort for Unavailable {
    fn call<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: SubagentRequest,
    ) -> PortFuture<'a, Result<SubagentAnswer, SubagentFailure>> {
        Box::pin(async {
            Err(SubagentFailure::Refused(format!(
                "delegation.session_closed: subagents are {UNAVAILABLE}"
            )))
        })
    }
}

impl CredentialsPort for Unavailable {
    fn resolve(&self, _: &str, _: &str) -> CredentialLookup {
        CredentialLookup::Unknown
    }
}

//! A bridged plugin's registrations as plugin-host contributions (spec:
//! python-bridge, Supported `PluginContext` subset; Hook name and payload
//! translation; Registrations follow plugin-system reload rules).
//!
//! - Tools join a session's registry under the toolset named after the
//!   plugin ([`BridgedTools`]); a session keeps the schemas it started
//!   with, and a call after a restart whose registrations lost the tool
//!   answers `tool '<name>' is no longer provided by plugin '<id>'`.
//! - Hooks run as external callbacks of the profile's hook registry
//!   ([`BridgedHook`]), bounded by `plugins.hook_timeout_ms`.
//! - Sections, slash commands and model-provider hooks run through
//!   [`BridgedBackend`], the plugin-host [`Backend`] of the sidecar.

use std::sync::Arc;

use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;
use pmt_plugins::backend::{Backend, ToolSource};
use pmt_plugins::grants::PluginGrants;
use pmt_plugins::hooks::{ExternalHook, HookFuture, Payload};
use pmt_plugins::host::CallInfo;
use pmt_plugins::host::hook_world::{Approval, Reply};
use pmt_plugins::host::surfaces_world::{ApprovalRequest, ApprovalResponse};
use pmt_plugins::providers::Category;
use pmt_plugins::tools::SessionCalls;
use pmt_tools::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolFailure, ToolHandler,
};
use pmt_tools::{Status, ToolRegistry, ToolResult, Toolset};
use serde_json::{Value, json};

use crate::sidecar::CallError;
use crate::supervisor::Supervisor;

/// The facts of a call as the shim reads them (`session_cwd()`...), with
/// the W3C `traceparent` of the session's current span (spec:
/// observability, Trace context propagation).
#[must_use]
pub fn context(call: &CallInfo, call_id: Option<&str>) -> Value {
    let traceparent = call
        .session_id
        .as_deref()
        .and_then(pmt_obs::otel::context::current)
        .map(|c| c.traceparent());
    json!({
        "session_id": call.session_id,
        "surface": call.surface,
        "cwd": call.cwd,
        "call_id": call_id,
        "traceparent": traceparent,
    })
}

/// The sidecar side of sections, commands and model hooks.
#[derive(Debug, Clone)]
pub struct BridgedBackend {
    supervisor: Arc<Supervisor>,
    grants: Arc<PluginGrants>,
}

impl BridgedBackend {
    /// The backend of `supervisor`'s plugin.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>, grants: Arc<PluginGrants>) -> Self {
        Self { supervisor, grants }
    }

    fn command(&self, params: &Value) -> Result<Result<Value, String>, Error> {
        let timeout = self.supervisor.settings().call_timeout;
        match self.supervisor.call("command.invoke", params, timeout) {
            Ok(answer) => Ok(Ok(answer)),
            Err(CallError::Remote { message, .. }) => Ok(Err(message)),
            Err(other) => Err(other.into_error()),
        }
    }

    fn provider(
        &self,
        provider: &str,
        method: &str,
        args: &Value,
    ) -> Result<Result<Value, String>, Error> {
        let timeout = self.supervisor.settings().call_timeout;
        let params = json!({"kind": "model", "provider": provider, "method": method, "args": args});
        match self.supervisor.call("provider.call", &params, timeout) {
            Ok(answer) => Ok(Ok(answer)),
            Err(CallError::Remote { message, .. }) => Ok(Err(message)),
            Err(other) => Err(other.into_error()),
        }
    }
}

impl Backend for BridgedBackend {
    fn id(&self) -> &str {
        self.supervisor.plugin()
    }

    fn granted(&self) -> CapabilitySet {
        self.grants.current()
    }

    fn render_section(&self, call: &CallInfo, id: &str) -> Result<Result<String, String>, Error> {
        let constant = self
            .supervisor
            .registrations()
            .and_then(|r| r.sections.into_iter().find(|s| s.id == id))
            .and_then(|s| s.r#static);
        if let Some(text) = constant {
            return Ok(Ok(text));
        }
        let info = json!({"session_id": call.session_id, "surface": call.surface, "cwd": call.cwd});
        let params =
            json!({"kind": "section", "name": id, "info": info, "context": context(call, None)});
        Ok(self
            .command(&params)?
            .map(|v| v.as_str().unwrap_or("").to_owned()))
    }

    fn run_command(
        &self,
        call: &CallInfo,
        name: &str,
        args: &str,
    ) -> Result<Result<String, String>, Error> {
        let params =
            json!({"kind": "slash", "name": name, "args": args, "context": context(call, None)});
        Ok(self
            .command(&params)?
            .map(|v| v.as_str().unwrap_or("").to_owned()))
    }

    fn model_headers(
        &self,
        call: &CallInfo,
        provider: &str,
    ) -> Result<Result<Vec<(String, String)>, String>, Error> {
        let args = json!({"context": {"session_id": call.session_id}});
        Ok(self.provider(provider, "headers", &args)?.map(|v| {
            v.as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned()))
                        .collect()
                })
                .unwrap_or_default()
        }))
    }

    fn prepare_request(
        &self,
        call: &CallInfo,
        provider: &str,
        body: &str,
    ) -> Result<Result<String, String>, Error> {
        let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let model = body["model"].clone();
        let args =
            json!({"body": body, "context": {"session_id": call.session_id, "model": model}});
        Ok(self
            .provider(provider, "prepare_request", &args)?
            .map(|v| v.to_string()))
    }

    fn present_approval(
        &self,
        call: &CallInfo,
        name: &str,
        request: &ApprovalRequest,
        bound: std::time::Duration,
    ) -> Result<Result<ApprovalResponse, String>, Error> {
        let request = json!({
            "request_id": request.id, "digest": request.digest, "command": request.command,
            "description": request.description, "surface": request.surface,
            "timeout_seconds": request.timeout_secs, "allowed_choices": request.choices,
        });
        let params = json!({"kind": "transport", "name": name, "request": request, "context": context(call, None)});
        let answer = match self.supervisor.call("command.invoke", &params, bound) {
            Ok(answer) => answer,
            Err(CallError::Remote { message, .. }) => return Ok(Err(message)),
            Err(other) => return Err(other.into_error()),
        };
        let text = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| answer[*k].as_str())
                .unwrap_or("")
                .to_owned()
        };
        Ok(Ok(ApprovalResponse {
            id: text(&["request_id", "id"]),
            digest: text(&["request_digest", "digest"]),
            choice: answer
                .as_str()
                .map_or_else(|| text(&["choice"]), str::to_owned),
        }))
    }

    fn invoke_provider(
        &self,
        call: &CallInfo,
        category: Category,
        id: &str,
        request: &str,
    ) -> Result<Result<String, String>, Error> {
        let request: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let (kind, method, args) = object_call(category, &request);
        let params = json!({"kind": kind, "provider": id, "method": method, "args": args, "context": context(call, None)});
        let timeout = self.supervisor.settings().call_timeout;
        let answer = match self.supervisor.call("provider.call", &params, timeout) {
            Ok(answer) => answer,
            Err(CallError::Remote { message, .. }) => return Ok(Err(message)),
            Err(other) => return Err(other.into_error()),
        };
        Ok(Ok(object_answer(category, answer).to_string()))
    }

    fn list_models(
        &self,
        _call: &CallInfo,
        provider: &str,
    ) -> Result<Result<Vec<String>, String>, Error> {
        Ok(self
            .provider(provider, "list_models", &json!({}))?
            .map(|v| {
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            }))
    }
}

/// The shim call that serves a provider-object request of `category`:
/// the provider kind, the Hermes method and its keyword arguments.
fn object_call(category: Category, request: &Value) -> (&'static str, &'static str, Value) {
    match category {
        Category::WebSearch => (
            "web_search",
            "search",
            json!({"query": request["query"], "limit": request["limit"]}),
        ),
        Category::WebExtract => ("web_search", "extract", json!({"urls": [request["url"]]})),
        Category::ImageGen => ("image_gen", "generate", request.clone()),
        Category::VideoGen => ("video_gen", "generate", request.clone()),
        Category::Tts => ("tts", "synthesize", request.clone()),
        Category::Stt => ("stt", "transcribe", request.clone()),
        Category::SecretSource => ("secret_source", "get", request.clone()),
    }
}

/// A Hermes provider's answer in the request shape of `category`: web
/// searches as `{"results": [{"title", "url", "snippet"}]}`, extracts as
/// `{"title", "markdown"}`; other categories as answered.
fn object_answer(category: Category, answer: Value) -> Value {
    let text = |v: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|k| v[*k].as_str())
            .unwrap_or("")
            .to_owned()
    };
    match category {
        Category::WebSearch => {
            let hits = answer["results"]
                .as_array()
                .or_else(|| answer.as_array())
                .cloned()
                .unwrap_or_default();
            let results: Vec<Value> = hits
                .iter()
                .map(|h| json!({"title": text(h, &["title"]), "url": text(h, &["url", "link"]), "snippet": text(h, &["snippet", "description", "content"])}))
                .collect();
            json!({"results": results})
        }
        Category::WebExtract => {
            let page = answer
                .as_array()
                .and_then(|a| a.first().cloned())
                .unwrap_or(answer);
            json!({"title": text(&page, &["title"]), "markdown": text(&page, &["content", "markdown", "raw_content"])})
        }
        _ => answer,
    }
}

/// A plugin's hook callbacks, after the WASM plugins'.
#[derive(Debug)]
pub struct BridgedHook {
    supervisor: Arc<Supervisor>,
    label: String,
}

impl BridgedHook {
    /// The callbacks of `supervisor`'s plugin.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        let label = format!("python:{}", supervisor.plugin());
        Self { supervisor, label }
    }
}

impl ExternalHook for BridgedHook {
    fn label(&self) -> &str {
        &self.label
    }

    fn call<'a>(
        &'a self,
        hook: &'a str,
        payload: &'a Payload,
        call: &'a CallInfo,
    ) -> HookFuture<'a> {
        let supervisor = Arc::clone(&self.supervisor);
        let params =
            json!({"hook": hook, "payload": payload.fields(), "context": context(call, None)});
        let hook = hook.to_owned();
        Box::pin(async move {
            let timeout = supervisor.settings().hook_timeout;
            let answer = tokio::task::spawn_blocking(move || {
                supervisor.call("hook.invoke", &params, timeout)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.message())?;
            Ok(answer["results"]
                .as_array()
                .and_then(|results| results.iter().find_map(|r| reply(&hook, r))))
        })
    }
}

/// A Hermes callback's return value as a hook reply.
#[must_use]
pub fn reply(hook: &str, value: &Value) -> Option<Reply> {
    match value {
        Value::String(text) => Some(Reply::Context(text.clone())),
        Value::Object(map) => {
            let text = |keys: &[&str]| {
                keys.iter()
                    .find_map(|k| map.get(*k).and_then(Value::as_str))
                    .map(str::to_owned)
            };
            match text(&["action", "decision"]).as_deref() {
                Some("block") => Some(Reply::Block(
                    text(&["message", "reason"]).unwrap_or_default(),
                )),
                Some("approve") => Some(Reply::Approve(Approval {
                    message: text(&["message"]).unwrap_or_default(),
                    rule_key: text(&["rule_key"]),
                })),
                Some("modify") => map
                    .get("args")
                    .or_else(|| map.get("arguments"))
                    .map(|v| Reply::Modify(v.to_string())),
                _ if hook == "pre_llm_call" => text(&["context"]).map(Reply::Context),
                _ => Some(Reply::Modify(value.to_string())),
            }
        }
        _ => None,
    }
}

/// A plugin's tools.
#[derive(Debug, Clone)]
pub struct BridgedTools {
    supervisor: Arc<Supervisor>,
    grants: Arc<PluginGrants>,
}

impl BridgedTools {
    /// The tools of `supervisor`'s plugin.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>, grants: Arc<PluginGrants>) -> Self {
        Self { supervisor, grants }
    }
}

struct BridgedTool {
    supervisor: Arc<Supervisor>,
    name: String,
    calls: SessionCalls,
}

impl ToolHandler for BridgedTool {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        let supervisor = Arc::clone(&self.supervisor);
        let name = self.name.clone();
        let calls = Arc::clone(&self.calls);
        let (session, call_id) = (context.session_id.clone(), context.call_id.clone());
        Box::pin(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                let facts = super::bridged::context(&calls(&session), Some(&call_id));
                supervisor.tool(&name, &arguments, &facts)
            })
            .await
            .map_err(|e| ToolFailure::new("panic", e.to_string()))?;
            Ok(match outcome {
                Ok(text) => ToolResult::ok(text),
                Err(err @ CallError::Timeout(_)) => {
                    ToolResult::text(Status::Timeout, err.message())
                }
                Err(err) => ToolResult::error(err.message()),
            })
        })
    }
}

impl ToolSource for BridgedTools {
    fn plugin(&self) -> &str {
        self.supervisor.plugin()
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        _base: &CallInfo,
        calls: &SessionCalls,
    ) -> Vec<String> {
        let id = self.supervisor.plugin().to_owned();
        let Some(registrations) = self.supervisor.registrations() else {
            return Vec::new();
        };
        if registrations.tools.is_empty() {
            return Vec::new();
        }
        let owner = Owner::Plugin(id.clone());
        let mut toolset = Toolset::new(&id, &format!("Tools of plugin '{id}'"), &[]);
        toolset.owner = owner.clone();
        if let Err(err) = registry.register_toolsets(&owner, vec![toolset]) {
            tracing::warn!(plugin = %id, "{}", err.message());
            return Vec::new();
        }
        let grants = self.grants.current();
        let mut registered = Vec::new();
        for tool in registrations.tools {
            let descriptor =
                ToolDescriptor::new(&tool.name, &id, tool.describe(), tool.parameters());
            let handler = Arc::new(BridgedTool {
                supervisor: Arc::clone(&self.supervisor),
                name: tool.name.clone(),
                calls: Arc::clone(calls),
            });
            if registry
                .register(
                    RegisteredTool::new(descriptor, owner.clone(), handler),
                    &grants,
                )
                .is_ok()
            {
                registered.push(tool.name);
            }
        }
        registered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_web_answers_take_the_provider_request_shape() {
        let hits =
            json!([{"title": "a", "url": "https://a", "description": "about a", "score": 1}]);
        assert_eq!(
            object_answer(Category::WebSearch, hits),
            json!({"results": [{"title": "a", "url": "https://a", "snippet": "about a"}]})
        );
        let pages = json!([{"url": "https://a", "title": "A", "content": "# A"}]);
        assert_eq!(
            object_answer(Category::WebExtract, pages),
            json!({"title": "A", "markdown": "# A"})
        );
        let (kind, method, args) = object_call(Category::WebExtract, &json!({"url": "https://a"}));
        assert_eq!(
            (kind, method, args),
            ("web_search", "extract", json!({"urls": ["https://a"]}))
        );
    }

    #[test]
    fn hermes_directives_become_hook_replies() {
        let block = reply(
            "pre_tool_call",
            &json!({"action": "block", "message": "no"}),
        );
        assert!(matches!(block, Some(Reply::Block(m)) if m == "no"));
        let modify = reply(
            "pre_tool_call",
            &json!({"action": "modify", "args": {"a": 1}}),
        );
        assert!(matches!(modify, Some(Reply::Modify(m)) if m == r#"{"a":1}"#));
        assert!(
            matches!(reply("transform_tool_result", &json!("new")), Some(Reply::Context(t)) if t == "new")
        );
        assert!(reply("post_tool_call", &Value::Null).is_none());
    }
}

/// Event deliveries to a plugin's `ctx.subscribe` callbacks
/// (`event.deliver`).
#[derive(Debug)]
pub struct BridgedEvents {
    supervisor: Arc<Supervisor>,
}

impl BridgedEvents {
    /// The deliveries of `supervisor`'s plugin.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }
}

impl pmt_plugins::events::EventSink for BridgedEvents {
    fn id(&self) -> &str {
        self.supervisor.plugin()
    }

    fn on_event(
        &self,
        call: &CallInfo,
        name: &str,
        payload: &str,
    ) -> Result<Result<(), String>, Error> {
        let payload: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        let params = json!({"event": name, "payload": payload, "context": context(call, None)});
        match self.supervisor.call(
            "event.deliver",
            &params,
            self.supervisor.settings().call_timeout,
        ) {
            Ok(_) => Ok(Ok(())),
            Err(CallError::Remote { message, .. }) => Ok(Err(message)),
            Err(other) => Err(other.into_error()),
        }
    }
}

/// A Hermes `MemoryProvider` the plugin registered, called in its sidecar
/// (spec: python-bridge, Scope of hosted code).
pub struct BridgedMemory {
    supervisor: Arc<Supervisor>,
    name: String,
}

impl BridgedMemory {
    /// Provider `name` of `supervisor`'s plugin.
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>, name: String) -> Self {
        Self { supervisor, name }
    }
}

impl pmt_plugins::memory::MemoryCalls for BridgedMemory {
    fn call(
        &self,
        method: &str,
        args: &Value,
        timeout: Option<std::time::Duration>,
    ) -> Result<Result<Value, String>, Error> {
        let timeout = timeout.unwrap_or_else(|| self.supervisor.settings().call_timeout);
        let params =
            json!({"kind": "memory", "provider": self.name, "method": method, "args": args});
        match self.supervisor.call("provider.call", &params, timeout) {
            Ok(answer) => Ok(Ok(answer)),
            Err(CallError::Remote { message, .. }) => Ok(Err(message)),
            Err(other) => Err(other.into_error()),
        }
    }
}

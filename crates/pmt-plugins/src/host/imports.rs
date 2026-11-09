//! The host interfaces a plugin imports (spec: plugin-system, Host
//! interfaces offered to plugins). `log`, `events` and `session` are always
//! available; `state` reaches only the instance profile's
//! `plugin-data/<id>/`; every other import checks the plugin's live grant
//! first and answers `capability_denied: <capability> <detail>` (also
//! logged) without reaching the port. Plugins get no environment, clock,
//! filesystem or network import beyond these.

use std::future::Future;
use std::sync::Arc;

use pmt_core::capability::{Capability, PluginRight};
use wasmtime::StoreLimits;

use super::bindings::prometheus::hook::console;
use super::bindings::prometheus::host::{
    config, events, http, llm, log, secrets, session, state, tools, types,
};
use super::bindings::prometheus::messaging::{conversation, inject, platform};
use super::ports::{
    HttpRequest, HttpResponse, InjectTarget, LlmCall, PlatformAction, Ports, PostFailure, Posted,
};
use super::state::{StateDir, StateError};
use crate::grants::PluginGrants;
use crate::manifest::label;

use types::HostError;

/// What one plugin instance may reach, fixed for its lifetime.
#[derive(Debug)]
pub struct Context {
    /// The plugin id.
    pub plugin: String,
    /// The profile the instance belongs to.
    pub profile: String,
    /// `<profile_home>/plugin-data/<id>/`.
    pub state: StateDir,
    /// Its live grant.
    pub grants: Arc<PluginGrants>,
    /// The embedder's services.
    pub ports: Ports,
}

/// Facts about the call in progress (`session.current`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallInfo {
    /// The session, when the call runs for one.
    pub session_id: Option<String>,
    /// `cli`, `tui`, `gateway`, `cron`...
    pub surface: String,
    /// The session's working directory.
    pub cwd: Option<String>,
}

/// The invoking terminal of a running `run-cli` command (hook world 1.3
/// `console`): what the plugin writes reaches it as it is written.
pub trait Console: Send + Sync {
    /// Bytes for the command's stdout.
    fn stdout(&self, bytes: &[u8]);
    /// Bytes for the command's stderr.
    fn stderr(&self, bytes: &[u8]);
}

/// The data of one wasmtime store.
pub(crate) struct HostState {
    pub(crate) ctx: Arc<Context>,
    pub(crate) call: CallInfo,
    pub(crate) limits: StoreLimits,
    /// Set only while a `run-cli` call runs.
    pub(crate) console: Option<Arc<dyn Console>>,
    /// The budget of the call in progress.
    pub(crate) meter: super::instance::Meter,
}

impl HostState {
    pub(super) fn require(&self, capability: &Capability) -> Result<(), HostError> {
        self.require_as(capability, &label(capability))
    }

    fn require_as(&self, capability: &Capability, what: &str) -> Result<(), HostError> {
        if self.ctx.grants.current().allows(capability) {
            return Ok(());
        }
        let message = format!("capability_denied: {what}");
        tracing::warn!(plugin = %self.ctx.plugin, profile = %self.ctx.profile, "{message}");
        Err(HostError::Denied(message))
    }

    pub(super) fn ports(&self) -> &Ports {
        &self.ctx.ports
    }
}

/// Runs a port future to completion for a host import. Plugin calls are
/// synchronous and may run on a blocking thread, an async worker (loading a
/// plugin inside a session start) or outside any runtime, so the future
/// runs on a short-lived thread of its own on the current runtime, or on a
/// private one when there is none.
pub(super) fn block_on<T: Send>(future: impl Future<Output = T> + Send) -> Result<T, HostError> {
    let failed = |e: &dyn std::fmt::Display| HostError::Failed(format!("host call failed: {e}"));
    let handle = tokio::runtime::Handle::try_current().ok();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || match handle {
            Some(handle) => Ok(handle.block_on(future)),
            None => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(future))
                .map_err(|e| failed(&e)),
        });
        worker
            .join()
            .unwrap_or_else(|_| Err(failed(&"the host call panicked")))
    })
}

fn state_error(error: StateError) -> HostError {
    match error {
        StateError::Invalid(m) => HostError::Invalid(m),
        StateError::Io(m) => HostError::Failed(m),
    }
}

impl types::Host for HostState {}

/// Outside a `run-cli` call there is no terminal: writes are dropped.
impl console::Host for HostState {
    fn write_stdout(&mut self, bytes: Vec<u8>) {
        if let Some(console) = &self.console {
            console.stdout(&bytes);
        }
    }

    fn write_stderr(&mut self, bytes: Vec<u8>) {
        if let Some(console) = &self.console {
            console.stderr(&bytes);
        }
    }
}

impl log::Host for HostState {
    fn log(&mut self, level: log::Level, message: String) {
        let plugin = &self.ctx.plugin;
        let profile = &self.ctx.profile;
        match level {
            log::Level::Trace => tracing::trace!(plugin, profile, "{message}"),
            log::Level::Debug => tracing::debug!(plugin, profile, "{message}"),
            log::Level::Info => tracing::info!(plugin, profile, "{message}"),
            log::Level::Warn => tracing::warn!(plugin, profile, "{message}"),
            log::Level::Error => tracing::error!(plugin, profile, "{message}"),
        }
    }
}

impl state::Host for HostState {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, HostError> {
        self.ctx.state.get(&key).map_err(state_error)
    }

    fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), HostError> {
        self.ctx.state.set(&key, &value).map_err(state_error)
    }

    fn delete(&mut self, key: String) -> Result<(), HostError> {
        self.ctx.state.delete(&key).map_err(state_error)
    }

    fn keys(&mut self) -> Result<Vec<String>, HostError> {
        self.ctx.state.keys().map_err(state_error)
    }

    fn read_file(&mut self, path: String) -> Result<Vec<u8>, HostError> {
        self.ctx.state.read_file(&path).map_err(state_error)
    }

    fn write_file(&mut self, path: String, contents: Vec<u8>) -> Result<(), HostError> {
        self.ctx
            .state
            .write_file(&path, &contents)
            .map_err(state_error)
    }
}

impl HostState {
    /// Sends one outbound request after the `network` grant check (the
    /// host's own name for the URL's host in the refusal), through the
    /// HTTP port and its egress policy.
    pub(super) fn send_http(
        &self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<HttpResponse, HostError> {
        let (host, port) =
            destination(&url).ok_or_else(|| HostError::Invalid(format!("invalid URL '{url}'")))?;
        let wanted = Capability::NetConnect {
            host: host.clone(),
            port: Some(port),
        };
        self.require_as(&wanted, &format!("network {host}"))?;
        let granted_hosts = self
            .ctx
            .grants
            .current()
            .iter()
            .filter_map(|c| match c {
                Capability::NetConnect { host, port } if host != "*" => {
                    let host = if host.contains(':') {
                        format!("[{host}]")
                    } else {
                        host.clone()
                    };
                    Some(port.map_or_else(|| host.clone(), |port| format!("{host}:{port}")))
                }
                _ => None,
            })
            .collect();
        let port_request = HttpRequest {
            method,
            url,
            headers,
            body,
            plugin: self.ctx.plugin.clone(),
            session_id: self.call.session_id.clone(),
            granted_hosts,
        };
        let http = Arc::clone(&self.ports().http);
        block_on(http.send(port_request))?.map_err(HostError::Failed)
    }
}

impl http::Host for HostState {
    fn send(&mut self, request: http::Request) -> Result<http::Response, HostError> {
        let response =
            self.send_http(request.method, request.url, request.headers, request.body)?;
        Ok(http::Response {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }
}

/// The lower-cased host and the port of an `http(s)` URL; `None` for
/// anything else (other schemes, user info, no host).
pub(super) fn destination(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') || authority.is_empty() {
        return None;
    }
    let (host, port) = pmt_core::capability::split_port(authority)?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_ascii_lowercase(), port.unwrap_or(default_port)))
}

impl secrets::Host for HostState {
    fn get(&mut self, name: String) -> Result<String, HostError> {
        let wanted = Capability::SecretsRead(name.clone());
        self.require_as(&wanted, &format!("secrets {name}"))?;
        self.ports()
            .secrets
            .get(&name)
            .ok_or_else(|| HostError::Failed(format!("secret {name} is not set")))
    }
}

impl HostState {
    fn llm_call(&self, request: llm::Request, schema: Option<String>) -> Result<String, HostError> {
        self.require_as(&Capability::LlmCall(None), "llm")?;
        if let Some(model) = &request.model {
            let wanted = Capability::Plugin {
                right: PluginRight::LlmModel,
                target: Some(model.clone()),
            };
            self.require_as(&wanted, "llm model override")?;
        }
        if request.provider.is_some() {
            let wanted = Capability::Plugin {
                right: PluginRight::LlmProviderOverride,
                target: None,
            };
            self.require_as(&wanted, "llm provider override")?;
        }
        let call = LlmCall {
            plugin: self.ctx.plugin.clone(),
            session_id: self.call.session_id.clone(),
            messages: request
                .messages
                .into_iter()
                .map(|m| (m.role, m.content))
                .collect(),
            model: request.model,
            provider: request.provider,
            max_tokens: request.max_tokens,
            schema,
        };
        let port = Arc::clone(&self.ports().llm);
        block_on(port.complete(call))?.map_err(HostError::Failed)
    }
}

impl llm::Host for HostState {
    fn complete(&mut self, request: llm::Request) -> Result<String, HostError> {
        self.llm_call(request, None)
    }

    fn complete_structured(
        &mut self,
        request: llm::Request,
        schema: String,
    ) -> Result<String, HostError> {
        self.llm_call(request, Some(schema))
    }
}

impl tools::Host for HostState {
    fn dispatch(&mut self, name: String, arguments: String) -> Result<String, HostError> {
        self.require(&Capability::ToolsCall(name.clone()))?;
        let port = Arc::clone(&self.ports().tools);
        let session = self.call.session_id.clone();
        block_on(port.dispatch(session, name, arguments))?.map_err(HostError::Failed)
    }
}

impl events::Host for HostState {
    fn emit(&mut self, name: String, payload: String) -> Result<(), HostError> {
        if name.is_empty() || name.contains(':') {
            return Err(HostError::Invalid(format!(
                "invalid event name '{name}': events are published as '<plugin>:<name>'; the name may not be empty or contain ':'"
            )));
        }
        let qualified = format!("{}:{name}", self.ctx.plugin);
        self.ports()
            .events
            .emit(&self.ctx.profile, &qualified, &payload);
        Ok(())
    }

    fn subscribe(&mut self, pattern: String) -> Result<(), HostError> {
        if pattern.is_empty() {
            return Err(HostError::Invalid("empty event pattern".to_owned()));
        }
        self.ports()
            .events
            .subscribe(&self.ctx.profile, &self.ctx.plugin, &pattern);
        Ok(())
    }
}

/// `plugins.entries.<id>.<key>`: one word of letters, digits and `_`.
fn check_key(key: &str) -> Result<(), HostError> {
    let valid = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(HostError::Invalid(format!(
            "invalid setting '{key}': lower-case letters, digits and '_'"
        )))
    }
}

impl config::Host for HostState {
    fn get(&mut self, key: String) -> Result<Option<String>, HostError> {
        check_key(&key)?;
        Ok(self
            .ports()
            .config
            .get(&self.ctx.plugin, &key)
            .map(|v| v.to_string()))
    }

    fn set(&mut self, key: String, value: String) -> Result<(), HostError> {
        check_key(&key)?;
        let value: serde_json::Value = serde_json::from_str(&value)
            .map_err(|e| HostError::Invalid(format!("setting values are JSON: {e}")))?;
        self.ports()
            .config
            .set(&self.ctx.plugin, &key, value)
            .map_err(HostError::Failed)
    }
}

impl session::Host for HostState {
    fn current(&mut self) -> session::Info {
        session::Info {
            session_id: self.call.session_id.clone(),
            profile: self.ctx.profile.clone(),
            surface: self.call.surface.clone(),
            cwd: self.call.cwd.clone(),
        }
    }
}

/// How long a live message may take before the plugin hears `timeout`.
const POST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl HostState {
    fn posted(
        send: impl Future<Output = Result<Posted, PostFailure>> + Send,
    ) -> Result<conversation::Posted, conversation::Failure> {
        let bounded = async move {
            tokio::time::timeout(POST_TIMEOUT, send)
                .await
                .unwrap_or(Err(PostFailure::Timeout))
        };
        match block_on(bounded) {
            Ok(Ok(posted)) => Ok(conversation::Posted {
                handle: posted.handle,
                fallback: posted.fallback,
            }),
            Ok(Err(failure)) => Err(wit_failure(failure)),
            Err(_) => Err(conversation::Failure::SendFailed),
        }
    }
}

fn wit_failure(failure: PostFailure) -> conversation::Failure {
    match failure {
        PostFailure::NoConversation => conversation::Failure::NoConversation,
        PostFailure::GatewayUnavailable => conversation::Failure::GatewayUnavailable,
        PostFailure::AdapterUnavailable => conversation::Failure::AdapterUnavailable,
        PostFailure::SendFailed => conversation::Failure::SendFailed,
        PostFailure::EditFailed => conversation::Failure::EditFailed,
        PostFailure::Timeout => conversation::Failure::Timeout,
    }
}

impl conversation::Host for HostState {
    fn post(&mut self, text: String) -> Result<conversation::Posted, conversation::Failure> {
        let Some(session) = self.call.session_id.clone() else {
            return Err(conversation::Failure::NoConversation);
        };
        let port = std::sync::Arc::clone(&self.ports().messaging);
        Self::posted(async move { port.post(&session, &text).await })
    }

    fn edit(
        &mut self,
        handle: String,
        text: String,
    ) -> Result<conversation::Posted, conversation::Failure> {
        let Some(session) = self.call.session_id.clone() else {
            return Err(conversation::Failure::NoConversation);
        };
        let port = std::sync::Arc::clone(&self.ports().messaging);
        Self::posted(async move { port.edit(&session, &handle, &text).await })
    }
}

impl inject::Host for HostState {
    fn inject(&mut self, content: String, role: String, target: inject::Target) -> bool {
        let refused = |why: &str| {
            tracing::warn!(plugin = %self.ctx.plugin, profile = %self.ctx.profile, "message injection refused: {why}");
            false
        };
        let right = Capability::Plugin {
            right: PluginRight::InjectMessages,
            target: None,
        };
        if self.require(&right).is_err() {
            return refused("capability_denied: inject_messages");
        }
        if role != "user" {
            return refused(&format!("role '{role}' is not `user`"));
        }
        if content.trim().is_empty() {
            return refused("empty content");
        }
        let target = match target {
            inject::Target::SessionKey(key) => InjectTarget::SessionKey(key),
            inject::Target::Origin(origin) => InjectTarget::Origin(origin),
        };
        let port = std::sync::Arc::clone(&self.ports().messaging);
        match block_on(async move { port.inject(&target, &content).await }) {
            Ok(Ok(())) => true,
            Ok(Err(why)) => refused(&why),
            Err(_) => refused("the host call failed"),
        }
    }
}

impl HostState {
    fn platform_action(&self, action: &PlatformAction) -> platform::Outcome {
        let verb = action.verb;
        let outcome = self.run_action(action.clone());
        tracing::info!(
            plugin = %self.ctx.plugin,
            verb,
            platform = %action.platform,
            outcome = outcome.error.as_deref().unwrap_or("ok"),
            "plugin platform action"
        );
        outcome
    }

    fn run_action(&self, action: PlatformAction) -> platform::Outcome {
        let verb = action.verb;
        let failed = |code: &str, detail: String| platform::Outcome {
            ok: false,
            action: verb.to_owned(),
            error: Some(code.to_owned()),
            detail: Some(detail),
        };
        let right = Capability::Plugin {
            right: PluginRight::PlatformAction,
            target: Some(verb.to_owned()),
        };
        if !self.ctx.grants.current().allows(&right) {
            return failed(
                "capability_not_granted",
                format!("platform_actions does not grant '{verb}'"),
            );
        }
        let fields = [
            &action.platform,
            &action.chat_id,
            &action.target_id,
            &action.value,
        ];
        if fields.iter().any(|f| f.trim().is_empty()) {
            return failed(
                "invalid_argument",
                "every argument must be non-empty".to_owned(),
            );
        }
        let port = std::sync::Arc::clone(&self.ports().messaging);
        match block_on(async move { port.platform_action(&action).await }) {
            Ok(Ok(())) => platform::Outcome {
                ok: true,
                action: verb.to_owned(),
                error: None,
                detail: None,
            },
            Ok(Err(err)) => failed(err.code, err.detail),
            Err(err) => failed("action_failed", format!("{err:?}")),
        }
    }
}

impl platform::Host for HostState {
    fn add_reaction(
        &mut self,
        platform: String,
        chat_id: String,
        message_id: String,
        emoji: String,
    ) -> platform::Outcome {
        self.platform_action(&PlatformAction {
            verb: "add_reaction",
            platform,
            chat_id,
            target_id: message_id,
            value: emoji,
        })
    }

    fn set_thread_title(
        &mut self,
        platform: String,
        chat_id: String,
        thread_id: String,
        title: String,
    ) -> platform::Outcome {
        self.platform_action(&PlatformAction {
            verb: "set_thread_title",
            platform,
            chat_id,
            target_id: thread_id,
            value: title,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::destination;

    #[test]
    fn destinations_are_host_and_port_of_http_urls_only() {
        assert_eq!(
            destination("https://API.example.com/v1?q=1"),
            Some(("api.example.com".to_owned(), 443))
        );
        assert_eq!(
            destination("http://h.example:8080"),
            Some(("h.example".to_owned(), 8080))
        );
        assert_eq!(destination("http://[::1]:9/x"), Some(("::1".to_owned(), 9)));
        for bad in ["ftp://h/x", "https://user@h/", "https:///x", "nonsense"] {
            assert_eq!(destination(bad), None, "{bad}");
        }
    }
}

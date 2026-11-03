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

use super::bindings::prometheus::host::{
    config, events, http, llm, log, secrets, session, state, tools, types,
};
use super::ports::{HttpRequest, LlmCall, Ports};
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

/// The data of one wasmtime store.
pub(crate) struct HostState {
    pub(crate) ctx: Arc<Context>,
    pub(crate) call: CallInfo,
    pub(crate) limits: StoreLimits,
}

impl HostState {
    fn require(&self, capability: &Capability) -> Result<(), HostError> {
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

    fn ports(&self) -> &Ports {
        &self.ctx.ports
    }
}

/// Runs a port future to completion for a host import. Plugin calls are
/// synchronous and may run on a blocking thread, an async worker (loading a
/// plugin inside a session start) or outside any runtime, so the future
/// runs on a short-lived thread of its own on the current runtime, or on a
/// private one when there is none.
fn block_on<T: Send>(future: impl Future<Output = T> + Send) -> Result<T, HostError> {
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

impl http::Host for HostState {
    fn send(&mut self, request: http::Request) -> Result<http::Response, HostError> {
        let (host, port) = destination(&request.url)
            .ok_or_else(|| HostError::Invalid(format!("invalid URL '{}'", request.url)))?;
        let wanted = Capability::NetConnect {
            host: host.clone(),
            port: Some(port),
        };
        self.require_as(&wanted, &format!("network {host}"))?;
        let port_request = HttpRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request.body,
        };
        let http = Arc::clone(&self.ports().http);
        let response = block_on(http.send(port_request))?.map_err(HostError::Failed)?;
        Ok(http::Response {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }
}

/// The lower-cased host and the port of an `http(s)` URL; `None` for
/// anything else (other schemes, user info, no host).
fn destination(url: &str) -> Option<(String, u16)> {
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

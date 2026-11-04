//! What a sidecar may ask of the host (`host.call`; spec: python-bridge,
//! Host-bound plugin APIs through the bridge; Secrets and environment
//! isolation; Supported `PluginContext` subset).
//!
//! Each request maps onto the same plugin-system host interface a WASM
//! plugin reaches ([`Ports`]), behind the same grant checks: model calls,
//! tool dispatch, the event bus, `plugins.entries.<id>.*`, live messages,
//! injection and platform actions, all bound to the calling session.
//! Plugin output of a `plugins run` command arrives here too. The
//! sidecar never holds an adapter handle or a secret it was not granted.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::capability::{Capability, PluginRight};
use pmt_plugins::grants::PluginGrants;
use pmt_plugins::host::Console;
use pmt_plugins::host::ports::{
    InjectTarget, LlmCall, PlatformAction, PortFuture, Ports, PostFailure, Posted,
};
use serde_json::{Value, json};

use crate::log::Log;
use crate::sidecar::{HostFault, HostHandler};

/// Reads lines from the invoking terminal (`plugins run`).
pub trait LineSource: Send + Sync {
    /// The next line with its newline, or `None` at end of input.
    fn read_line(&self) -> Option<String>;
}

/// The command console of a one-shot `plugins run` sidecar.
pub struct Terminal {
    /// Where output goes.
    pub output: Arc<dyn Console>,
    /// Where input comes from, when interactive.
    pub input: Option<Arc<dyn LineSource>>,
}

/// One plugin's host side.
pub struct BridgeHost {
    plugin: String,
    profile: String,
    grants: Arc<PluginGrants>,
    ports: Ports,
    runtime: Option<tokio::runtime::Handle>,
    log: Arc<Log>,
    loaded: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    terminal: Mutex<Option<Terminal>>,
    events: Mutex<Option<Arc<dyn pmt_plugins::host::ports::EventsPort>>>,
}

impl BridgeHost {
    /// The host of `plugin` in `profile`; `runtime` runs the ports'
    /// futures (a private one per call when `None`); `loaded` answers
    /// `ctx.has_plugin`.
    #[must_use]
    pub fn new(
        plugin: &str,
        profile: &str,
        grants: Arc<PluginGrants>,
        ports: Ports,
        runtime: Option<tokio::runtime::Handle>,
        log: Arc<Log>,
        loaded: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            plugin: plugin.to_owned(),
            profile: profile.to_owned(),
            grants,
            ports,
            runtime,
            log,
            loaded,
            terminal: Mutex::new(None),
            events: Mutex::new(None),
        }
    }

    /// Routes `ctx.emit` to `bus` (the profile's current event bus).
    pub fn set_events(&self, bus: Arc<dyn pmt_plugins::host::ports::EventsPort>) {
        *self.events.lock().unwrap_or_else(PoisonError::into_inner) = Some(bus);
    }

    /// Attaches the terminal of a `plugins run` command.
    pub fn attach(&self, terminal: Terminal) {
        *self.terminal.lock().unwrap_or_else(PoisonError::into_inner) = Some(terminal);
    }

    fn block<T>(&self, future: PortFuture<'_, T>) -> Result<T, HostFault> {
        match &self.runtime {
            Some(handle) => Ok(handle.block_on(future)),
            None => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| rt.block_on(future))
                .map_err(|e| HostFault::new("unavailable", e.to_string())),
        }
    }

    fn allows(&self, capability: &Capability) -> bool {
        self.grants.current().allows(capability)
    }

    fn require(&self, capability: &Capability, label: &str) -> Result<(), HostFault> {
        if self.allows(capability) {
            return Ok(());
        }
        let text = format!("capability_denied: {label}");
        self.log.note(&text);
        Err(HostFault::new("capability_denied", text))
    }

    fn config(&self, method: &str, args: &Value) -> Result<Value, HostFault> {
        let key = args["key"].as_str().unwrap_or("");
        if key.is_empty() {
            return Err(HostFault::new(
                "invalid_argument",
                "a config key is required",
            ));
        }
        if method == "config.get" {
            return Ok(self
                .ports
                .config
                .get(&self.plugin, key)
                .unwrap_or(Value::Null));
        }
        self.ports
            .config
            .set(&self.plugin, key, args["value"].clone())
            .map(|()| Value::Null)
            .map_err(|e| HostFault::new("config", e))
    }

    fn llm(&self, args: &Value, context: &Value) -> Result<Value, HostFault> {
        self.require(&Capability::LlmCall(None), "llm")?;
        let model = args["model"].as_str().map(str::to_owned);
        if let Some(model) = &model {
            let right = Capability::Plugin {
                right: PluginRight::LlmModel,
                target: Some(model.clone()),
            };
            self.require(&right, &format!("llm model override '{model}'"))?;
        }
        let provider = args["provider"].as_str().map(str::to_owned);
        if provider.is_some() {
            let right = Capability::Plugin {
                right: PluginRight::LlmProviderOverride,
                target: None,
            };
            self.require(&right, "llm provider override")?;
        }
        let messages = args["messages"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|m| {
                        (
                            m["role"].as_str().unwrap_or("user").to_owned(),
                            m["content"].as_str().unwrap_or("").to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let call = LlmCall {
            plugin: self.plugin.clone(),
            session_id: context["session_id"].as_str().map(str::to_owned),
            messages,
            model,
            provider,
            max_tokens: args["max_tokens"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok()),
            schema: args["schema"].as_str().map(str::to_owned),
        };
        let port = Arc::clone(&self.ports.llm);
        self.block(Box::pin(async move { port.complete(call).await }))?
            .map(Value::String)
            .map_err(|e| HostFault::new("llm", e))
    }

    fn dispatch(&self, args: &Value, context: &Value) -> Result<Value, HostFault> {
        let name = args["name"].as_str().unwrap_or("").to_owned();
        self.require(
            &Capability::ToolsCall(name.clone()),
            &format!("tools_call {name}"),
        )?;
        let port = Arc::clone(&self.ports.tools);
        let session = context["session_id"].as_str().map(str::to_owned);
        let arguments = args["arguments"].to_string();
        self.block(Box::pin(async move {
            port.dispatch(session, name, arguments).await
        }))?
        .map(Value::String)
        .map_err(|e| HostFault::new("tool", e))
    }

    fn emit(&self, args: &Value) -> Value {
        let name = format!("{}:{}", self.plugin, args["name"].as_str().unwrap_or(""));
        let bus = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let port = bus.unwrap_or_else(|| Arc::clone(&self.ports.events));
        port.emit(&self.profile, &name, &args["payload"].to_string());
        json!(0)
    }

    fn post(&self, method: &str, args: &Value) -> Result<Value, HostFault> {
        let session = args["session_id"].as_str().unwrap_or("").to_owned();
        let text = args["text"].as_str().unwrap_or("").to_owned();
        let port = Arc::clone(&self.ports.messaging);
        let posted: Result<Posted, PostFailure> = if method == "messaging.post" {
            self.block(Box::pin(async move { port.post(&session, &text).await }))?
        } else {
            let handle = args["handle"].as_str().unwrap_or("").to_owned();
            self.block(Box::pin(async move {
                port.edit(&session, &handle, &text).await
            }))?
        };
        posted
            .map(|p| json!({"handle": p.handle, "fallback": p.fallback}))
            .map_err(|failure| HostFault::new(failure_code(failure), format!("{failure:?}")))
    }

    fn inject(&self, args: &Value) -> Result<Value, HostFault> {
        let right = Capability::Plugin {
            right: PluginRight::InjectMessages,
            target: None,
        };
        if !self.allows(&right) {
            self.log
                .note("message injection refused: capability_denied: inject_messages");
            return Ok(json!(false));
        }
        let content = args["content"].as_str().unwrap_or("").to_owned();
        if args["role"].as_str().unwrap_or("user") != "user" || content.trim().is_empty() {
            self.log
                .note("message injection refused: only non-empty `user` messages are injected");
            return Ok(json!(false));
        }
        let target = match (args["session_key"].as_str(), &args["origin"]) {
            (Some(key), _) => InjectTarget::SessionKey(key.to_owned()),
            (None, Value::String(origin)) => InjectTarget::Origin(origin.clone()),
            (None, Value::Object(origin)) => InjectTarget::Origin(format!(
                "{}:{}",
                origin.get("platform").and_then(Value::as_str).unwrap_or(""),
                origin.get("chat_id").and_then(Value::as_str).unwrap_or("")
            )),
            _ => return Ok(json!(false)),
        };
        let port = Arc::clone(&self.ports.messaging);
        match self.block(Box::pin(
            async move { port.inject(&target, &content).await },
        ))? {
            Ok(()) => Ok(json!(true)),
            Err(why) => {
                self.log.note(&format!("message injection refused: {why}"));
                Ok(json!(false))
            }
        }
    }

    fn platform_action(&self, args: &Value) -> Result<Value, HostFault> {
        let verb: &'static str = match args["verb"].as_str() {
            Some("add_reaction") => "add_reaction",
            Some("set_thread_title") => "set_thread_title",
            _ => return Ok(json!({"ok": false, "error": "invalid_argument"})),
        };
        let right = Capability::Plugin {
            right: PluginRight::PlatformAction,
            target: Some(verb.to_owned()),
        };
        if !self.allows(&right) {
            self.log.note(&format!(
                "platform action refused: platform_actions does not grant '{verb}'"
            ));
            return Ok(json!({"ok": false, "action": verb, "error": "capability_not_granted"}));
        }
        let field = |key: &str| args[key].as_str().unwrap_or("").to_owned();
        let action = PlatformAction {
            verb,
            platform: field("platform"),
            chat_id: field("chat_id"),
            target_id: field("target_id"),
            value: field("value"),
        };
        let port = Arc::clone(&self.ports.messaging);
        Ok(
            match self.block(Box::pin(async move { port.platform_action(&action).await }))? {
                Ok(()) => json!({"ok": true, "action": verb}),
                Err(err) => {
                    json!({"ok": false, "action": verb, "error": err.code, "detail": err.detail})
                }
            },
        )
    }

    fn console(&self, method: &str, args: &Value) -> Result<Value, HostFault> {
        let terminal = self.terminal.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(terminal) = terminal.as_ref() else {
            return Err(HostFault::new("no_terminal", "no terminal is attached"));
        };
        if method == "console.readline" {
            return Ok(terminal
                .input
                .as_ref()
                .and_then(|i| i.read_line())
                .map_or(json!(""), Value::String));
        }
        let text = args["text"].as_str().unwrap_or("");
        if args["stream"] == "stderr" {
            terminal.output.stderr(text.as_bytes());
        } else {
            terminal.output.stdout(text.as_bytes());
        }
        Ok(Value::Null)
    }
}

/// The handle `post_message` exposes for a failure.
fn failure_code(failure: PostFailure) -> &'static str {
    match failure {
        PostFailure::NoConversation => "no_conversation",
        PostFailure::GatewayUnavailable => "gateway_unavailable",
        PostFailure::AdapterUnavailable => "adapter_unavailable",
        PostFailure::SendFailed => "send_failed",
        PostFailure::EditFailed => "edit_failed",
        PostFailure::Timeout => "timeout",
    }
}

impl HostHandler for BridgeHost {
    fn handle(&self, method: &str, args: Value, context: &Value) -> Result<Value, HostFault> {
        match method {
            "config.get" | "config.set" => self.config(method, &args),
            "llm.complete" => self.llm(&args, context),
            "tools.dispatch" => self.dispatch(&args, context),
            "events.emit" => Ok(self.emit(&args)),
            "messaging.post" | "messaging.edit" => self.post(method, &args),
            "messaging.inject" => self.inject(&args),
            "messaging.platform_action" => self.platform_action(&args),
            "plugins.has" => Ok(json!((self.loaded)(args["id"].as_str().unwrap_or("")))),
            "console.write" | "console.readline" => self.console(method, &args),
            other => Err(HostFault::new(
                "unknown_method",
                format!("unknown host method '{other}'"),
            )),
        }
    }
}

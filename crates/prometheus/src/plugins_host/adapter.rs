//! WASM `platform-adapter` plugins as messaging adapters (spec:
//! platform-adapters, Adapter contract; First-party platforms and typed
//! platform names; WASM adapter plugin isolation).
//!
//! The gateway asks [`factory`] for every enabled platform no first-party
//! adapter serves; it answers with the adapter of the profile's loaded
//! plugin that provides the platform (the plugin id, lower case). The
//! plugin runs under its own grants like any plugin call: an undeclared
//! host is refused by the host imports (and logged naming the plugin).
//! Inbound messages are polled; a trap or timeout of a plugin call puts
//! only this adapter into `retrying` (the gateway reconnects it with
//! backoff, which instantiates the plugin afresh), and five crashes within
//! ten minutes make it `fatal`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_gateway::messaging::probe::Reach;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, AdapterState,
    ChatInfo, ChatType, Descriptor, HttpRequest, HttpResponse, Inbound, Outgoing, SendError,
    SenderVerdict, Sent,
};
use pmt_plugins::host::platform_adapter_world as wit;
use pmt_plugins::host::{CallInfo, Instance};
use serde_json::{Value, json};

/// How often inbound messages are polled.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// Crashes within [`CRASH_WINDOW`] that stop the adapter for good.
const CRASHES_TO_FATAL: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_mins(10);
/// How often the profile's plugin set is re-checked when an adapter is
/// built.
const RECHECK: Duration = Duration::from_secs(5);

/// Declares the platform of every enabled `platform-adapter` plugin of
/// the home's profiles (spec: platform-adapters, First-party platforms and
/// typed platform names), so a `[gateway.platforms.<name>]` table naming
/// one validates: read once per process, from the profiles'
/// `plugins.enabled` lists and the plugins' manifests, before any
/// profile's configuration is checked. The platform is the plugin id,
/// lower case.
pub fn register_platforms(home: &Path) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for platform in enabled_adapters(home) {
            // A plugin publishes no options schema: its own `start`
            // checks the options it is given.
            let any = serde_json::json!({"type": "object", "additionalProperties": true});
            pmt_config::platforms::register_plugin_platform(&platform, Some(any));
        }
    });
}

fn toml_of(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

fn strings(table: Option<&toml::Value>) -> Vec<String> {
    table
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// The ids (lower case) of the enabled adapter plugins of every profile.
fn enabled_adapters(home: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let profiles = std::fs::read_dir(home.join("profiles"))
        .into_iter()
        .flatten()
        .flatten();
    for profile in profiles {
        let Some(config) = toml_of(&profile.path().join("config.toml")) else {
            continue;
        };
        let plugins = config.get("plugins");
        let disabled = strings(plugins.and_then(|p| p.get("disabled")));
        for id in strings(plugins.and_then(|p| p.get("enabled"))) {
            let manifest = profile.path().join("plugins").join(&id).join("plugin.toml");
            let adapter = toml_of(&manifest).is_some_and(|m| {
                strings(m.get("worlds"))
                    .iter()
                    .any(|w| w.starts_with("platform-adapter@"))
            });
            if adapter && !disabled.contains(&id) {
                found.push(id.to_lowercase());
            }
        }
    }
    found
}

/// Builds the adapter of a platform an enabled plugin of the profile
/// provides.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup: &AdapterSetup| {
        let plugins = super::current(&setup.scope, RECHECK).current();
        let instance = plugins
            .plugins
            .iter()
            .find(|p| p.manifest.adapter_platform().as_deref() == Some(setup.platform.as_str()))
            .map(|p| Arc::clone(&p.instance))
            .ok_or_else(|| {
                Error::new(
                    &codes::PLUGINS_LOAD,
                    format!(
                        "no enabled adapter plugin of profile '{}' provides platform '{}'",
                        setup.scope.name(),
                        setup.platform
                    ),
                )
                .with_hint("prometheus plugins list shows why an adapter plugin did not load")
            })?;
        let options = Value::Object(setup.config.options.clone().into_iter().collect());
        Ok(Arc::new(PluginAdapter::read(instance, &setup.platform, &options)?) as Arc<dyn Adapter>)
    })
}

/// One plugin's adapter.
struct PluginAdapter {
    instance: Arc<Instance>,
    descriptor: Descriptor,
    options: String,
    call: CallInfo,
    link: Mutex<Option<Arc<dyn AdapterLink>>>,
    poller: Mutex<Option<tokio::task::JoinHandle<()>>>,
    crashes: Arc<Mutex<VecDeque<Instant>>>,
}

impl PluginAdapter {
    /// The adapter of `instance` for `platform`: its descriptor is read
    /// (and checked) now, before the gateway connects it.
    fn read(instance: Arc<Instance>, platform: &str, options: &Value) -> Result<Self, Error> {
        let call = CallInfo::default();
        let text = instance.adapter_descriptor(&call)?;
        let invalid = |why: String| {
            Error::new(
                &codes::PLUGINS_LOAD,
                format!("plugin '{}': {why}", instance.id()),
            )
        };
        let mut value: Value = serde_json::from_str(&text)
            .map_err(|e| invalid(format!("its descriptor is not JSON: {e}")))?;
        let named = value["platform"].as_str().unwrap_or_default().to_owned();
        if named.to_lowercase() != platform {
            return Err(invalid(format!(
                "its descriptor names platform '{named}', not '{platform}'"
            )));
        }
        // Platform names are case-normalized to lower case.
        value["platform"] = Value::String(platform.to_owned());
        let descriptor = Descriptor::from_value(&value).map_err(invalid)?;
        Ok(Self {
            instance,
            descriptor,
            options: options.to_string(),
            call,
            link: Mutex::default(),
            poller: Mutex::default(),
            crashes: Arc::default(),
        })
    }

    /// Runs a plugin call on the blocking pool (calls are synchronous and
    /// may wait on host imports).
    async fn blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&Instance, &CallInfo) -> Result<R, Error> + Send + 'static,
    ) -> Result<R, Error> {
        let (instance, call) = (Arc::clone(&self.instance), self.call.clone());
        pmt_core::spawn::blocking(move || f(&instance, &call))
            .await
            .map_err(|e| Error::new(&codes::PLUGINS_CALL, format!("plugin call: {e}")))?
    }

    /// Reports a trap or timeout to the gateway (when connected).
    fn crashed(&self, err: &Error) {
        let state = record_crash(&self.crashes);
        let link = lock(&self.link).clone();
        if let Some(link) = link {
            link.report(state, Some(crash_error(err)));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records one crash: `fatal` once five fall within ten minutes, else
/// `retrying`.
fn record_crash(crashes: &Mutex<VecDeque<Instant>>) -> AdapterState {
    let now = Instant::now();
    let mut crashes = lock(crashes);
    crashes.push_back(now);
    while crashes
        .front()
        .is_some_and(|at| now.duration_since(*at) > CRASH_WINDOW)
    {
        crashes.pop_front();
    }
    if crashes.len() >= CRASHES_TO_FATAL {
        AdapterState::Fatal
    } else {
        AdapterState::Retrying
    }
}

fn crash_error(err: &Error) -> AdapterError {
    AdapterError {
        retryable: true,
        code: "plugin_crashed".to_owned(),
        message: err.message().to_owned(),
    }
}

/// One polled message: `{id, chat, chat_type, sender, text, thread}`.
fn inbound(value: &Value) -> Inbound {
    let text = |key: &str| value[key].as_str().map(str::to_owned);
    let chat_type = value["chat_type"]
        .as_str()
        .and_then(ChatType::parse)
        .unwrap_or(ChatType::Dm);
    let mut inbound = Inbound::text(
        chat_type,
        &text("chat").unwrap_or_default(),
        &text("sender").unwrap_or_default(),
        &text("text").unwrap_or_default(),
    );
    inbound.message_id = text("id").unwrap_or_default();
    inbound.thread_id = text("thread");
    inbound.sender_name = text("sender_name");
    inbound
}

/// Polls until the plugin fails or traps; the gateway reconnects.
async fn poll(
    instance: Arc<Instance>,
    call: CallInfo,
    link: Arc<dyn AdapterLink>,
    crashes: Arc<Mutex<VecDeque<Instant>>>,
) {
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let (plugin, call) = (Arc::clone(&instance), call.clone());
        let polled = pmt_core::spawn::blocking(move || plugin.adapter_poll(&call)).await;
        let messages = match polled {
            Ok(Ok(Ok(text))) => text,
            Ok(Ok(Err(why))) => {
                let error = AdapterError {
                    retryable: true,
                    code: "plugin_poll".to_owned(),
                    message: why,
                };
                link.report(AdapterState::Retrying, Some(error));
                return;
            }
            Ok(Err(err)) => {
                let state = record_crash(&crashes);
                link.report(state, Some(crash_error(&err)));
                return;
            }
            Err(join) => {
                let err = Error::new(&codes::PLUGINS_CALL, format!("plugin poll: {join}"));
                link.report(AdapterState::Retrying, Some(crash_error(&err)));
                return;
            }
        };
        let values: Vec<Value> = serde_json::from_str(&messages).unwrap_or_else(|e| {
            tracing::warn!(plugin = %instance.id(), "the adapter polled malformed messages: {e}");
            Vec::new()
        });
        for value in &values {
            link.submit(inbound(value)).await;
        }
    }
}

/// A send failure the plugin names: `permanent: ...` is never retried,
/// anything else is.
fn send_error(why: String) -> SendError {
    match why.strip_prefix("permanent:") {
        Some(rest) => SendError::Permanent(rest.trim().to_owned()),
        None => SendError::Network(why),
    }
}

impl Adapter for PluginAdapter {
    fn describe(&self) -> Descriptor {
        self.descriptor.clone()
    }

    fn reach(&self) -> Result<Reach, String> {
        // The plugin reaches its hosts through the host imports under its
        // grants; the host has no endpoint of its own to probe.
        Ok(Reach::InboundOnly)
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(async move {
            let options = self.options.clone();
            match self
                .blocking(move |instance, call| instance.adapter_start(call, &options))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(why)) => {
                    return Err(AdapterError {
                        retryable: true,
                        code: "plugin_start".to_owned(),
                        message: why,
                    });
                }
                Err(err) => {
                    record_crash(&self.crashes);
                    return Err(crash_error(&err));
                }
            }
            *lock(&self.link) = Some(Arc::clone(&link));
            let task = pmt_core::spawn::spawn(poll(
                Arc::clone(&self.instance),
                self.call.clone(),
                link,
                Arc::clone(&self.crashes),
            ));
            if let Some(old) = lock(&self.poller).replace(task) {
                old.abort();
            }
            Ok(())
        })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            if let Some(task) = lock(&self.poller).take() {
                task.abort();
            }
            lock(&self.link).take();
            if let Err(err) = self.blocking(Instance::adapter_stop).await {
                tracing::warn!(plugin = %self.instance.id(), "the adapter's stop failed: {}", err.message());
            }
        })
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let body = json!({
            "chat": message.chat_id,
            "thread": message.thread_id,
            "reply_to": message.reply_to,
            "text": message.text,
        })
        .to_string();
        Box::pin(async move {
            match self
                .blocking(move |instance, call| instance.adapter_send(call, &body))
                .await
            {
                Ok(Ok(id)) => Ok(Sent {
                    message_id: Some(id),
                }),
                Ok(Err(why)) => Err(send_error(why)),
                Err(err) => {
                    self.crashed(&err);
                    Err(SendError::Network(err.message().to_owned()))
                }
            }
        })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        let chat = chat_id.to_owned();
        Box::pin(async move {
            let text = match self
                .blocking(move |instance, call| instance.adapter_chat_info(call, &chat))
                .await
            {
                Ok(Ok(text)) => text,
                Ok(Err(why)) => return Err(send_error(why)),
                Err(err) => {
                    self.crashed(&err);
                    return Err(SendError::Network(err.message().to_owned()));
                }
            };
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| SendError::Permanent(format!("malformed chat info: {e}")))?;
            Ok(ChatInfo {
                name: value["name"].as_str().unwrap_or(chat_id).to_owned(),
                chat_type: value["chat_type"]
                    .as_str()
                    .and_then(ChatType::parse)
                    .unwrap_or(ChatType::Dm),
            })
        })
    }

    fn authorize_sender<'a>(
        &'a self,
        inbound: &'a Inbound,
    ) -> AdapterFuture<'a, Result<SenderVerdict, AdapterError>> {
        let body = json!({
            "sender": inbound.sender_id,
            "chat": inbound.chat_id,
            "text": inbound.text,
        })
        .to_string();
        Box::pin(async move {
            let verdict = self
                .blocking(move |instance, call| instance.adapter_authorize(call, &body))
                .await
                .map_err(|err| crash_error(&err))?;
            Ok(match verdict.as_str() {
                "allow" => SenderVerdict::Allow,
                "deny" => SenderVerdict::Deny,
                _ => SenderVerdict::Unknown,
            })
        })
    }

    fn verify_http(&self, _request: &HttpRequest) -> bool {
        // The plugin checks its platform's signature in `handle-http`.
        self.descriptor.binds_http
    }

    fn handle_http<'a>(&'a self, request: &'a HttpRequest) -> AdapterFuture<'a, HttpResponse> {
        let path = match &request.query {
            Some(query) => format!("{}?{query}", request.path),
            None => request.path.clone(),
        };
        let request = wit::HttpRequest {
            method: request.method.clone(),
            path,
            headers: request.headers.clone(),
            body: request.body.clone(),
        };
        Box::pin(async move {
            match self
                .blocking(move |instance, call| instance.adapter_handle_http(call, &request))
                .await
            {
                Ok(answer) => HttpResponse {
                    status: answer.status,
                    content_type: None,
                    headers: answer.headers,
                    body: answer.body,
                },
                Err(err) => {
                    self.crashed(&err);
                    HttpResponse::status(500)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fifth_crash_within_ten_minutes_is_fatal() {
        let crashes = Mutex::default();
        for _ in 0..4 {
            assert_eq!(record_crash(&crashes), AdapterState::Retrying);
        }
        assert_eq!(record_crash(&crashes), AdapterState::Fatal);
    }

    #[test]
    fn only_a_permanent_send_failure_is_not_retried() {
        assert_eq!(
            send_error("permanent: chat archived".to_owned()),
            SendError::Permanent("chat archived".to_owned())
        );
        assert_eq!(
            send_error("timed out".to_owned()),
            SendError::Network("timed out".to_owned())
        );
    }
}

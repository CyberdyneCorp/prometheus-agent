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
//! ten minutes make it `fatal`; a poll error `fatal: <code>: ..` makes it
//! `fatal` at once. Files cross the boundary as base64 (`adapter_media`);
//! edits, deletes, reactions and typing go through `send` with an `op`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_gateway::messaging::probe::Reach;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, AdapterState,
    Attachment, ChatInfo, ChatType, Descriptor, HttpRequest, HttpResponse, Inbound, MediaStream,
    Outgoing, SendError, SenderVerdict, Sent,
};
use pmt_plugins::host::platform_adapter_world as wit;
use pmt_plugins::host::{CallInfo, Instance};
use serde_json::{Value, json};

use super::adapter_media::{self, Inbox};
use super::sidecar;

/// How often inbound messages are polled.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// Crashes within [`CRASH_WINDOW`] that stop the adapter for good.
const CRASHES_TO_FATAL: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_mins(10);
/// How often the profile's plugin set is re-checked when an adapter is
/// built.
const RECHECK: Duration = Duration::from_secs(5);

/// Declares the platform of every installed `platform-adapter` plugin of
/// the home's profiles (spec: platform-adapters, First-party platforms and
/// typed platform names), so a `[gateway.platforms.<name>]` table naming
/// one validates: read once per process, from the plugins' manifests,
/// before any profile's configuration is checked. A disabled plugin counts
/// (a migrated catalog adapter arrives disabled with its table; the
/// adapter itself starts only once the plugin is enabled). The platform is
/// the plugin id, lower case.
pub fn register_platforms(home: &Path) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for platform in installed_adapters(home) {
            register(&platform);
        }
    });
}

/// Declares the platform of plugin `id` just installed in the profile at
/// `profile_dir`, when it is an adapter.
pub fn register_installed(profile_dir: &Path, id: &str) {
    if is_adapter(&profile_dir.join("plugins").join(id)) {
        register(&id.to_lowercase());
    }
}

fn register(platform: &str) {
    // A plugin publishes no options schema: its own `start` checks the
    // options it is given.
    let any = serde_json::json!({"type": "object", "additionalProperties": true});
    pmt_config::platforms::register_plugin_platform(platform, Some(any));
}

fn toml_of(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

fn is_adapter(dir: &Path) -> bool {
    toml_of(&dir.join("plugin.toml")).is_some_and(|m| {
        m.get("worlds")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .any(|w| w.starts_with("platform-adapter@"))
    })
}

/// The ids (lower case) of the installed adapter plugins of every profile.
fn installed_adapters(home: &Path) -> Vec<String> {
    let plugin_dirs = std::fs::read_dir(home.join("profiles"))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|profile| {
            std::fs::read_dir(profile.path().join("plugins"))
                .into_iter()
                .flatten()
                .flatten()
        });
    plugin_dirs
        .filter(|plugin| is_adapter(&plugin.path()))
        .map(|plugin| plugin.file_name().to_string_lossy().to_lowercase())
        .collect()
}

/// Builds the adapter of a platform an enabled plugin of the profile
/// provides.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup: &AdapterSetup| {
        let plugins = super::current(&setup.scope, RECHECK).current();
        let plugin = plugins
            .plugins
            .iter()
            .find(|p| p.manifest.adapter_platform().as_deref() == Some(setup.platform.as_str()))
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
        let mut options = Value::Object(setup.config.options.clone().into_iter().collect());
        options["access"] = access_of(&setup.config);
        let mut adapter =
            PluginAdapter::read(Arc::clone(&plugin.instance), &setup.platform, &options)?;
        adapter.sidecar = sidecar_of(setup, plugin);
        adapter.media = Arc::new(Inbox::granted(Arc::clone(&plugin.grants)));
        Ok(Arc::new(adapter) as Arc<dyn Adapter>)
    })
}

/// The platform table's allowlists, handed to the plugin as
/// `options.access` (`{allow_users, allow_chats, allow_all}`): the gateway
/// still enforces them, but some platforms behave differently without one
/// (ntfy then only publishes) or take ids of one shape only (Mattermost
/// user ids, `SimpleX` contact ids), which the plugin checks at start.
fn access_of(config: &pmt_config::schema::PlatformConfig) -> Value {
    json!({
        "allow_users": config.allow_users,
        "allow_chats": config.allow_chats,
        "allow_all": config.allow_all,
    })
}

/// What starts `plugin`'s sidecar, when it declares one.
fn sidecar_of(setup: &AdapterSetup, plugin: &pmt_plugins::load::LoadedPlugin) -> Option<Sidecar> {
    let declaration = plugin.manifest.sidecar.clone()?;
    let Some(dir) = plugin.dir.clone() else {
        tracing::warn!(
            plugin = plugin.manifest.id,
            "an embedded plugin cannot run a sidecar"
        );
        return None;
    };
    let grants = plugin.grants.current();
    let names = sidecar::granted_secrets(&declaration, &grants);
    Some(Sidecar {
        spec: sidecar::Spec {
            home: setup.scope.home().to_path_buf(),
            profile: setup.scope.name().as_str().to_owned(),
            plugin: plugin.manifest.id.clone(),
            plugin_dir: dir,
            declaration,
            secrets: Vec::new(),
            node: None,
            network: sidecar::granted_network(&grants),
        },
        names,
        scope: setup.scope.clone(),
        running: Mutex::default(),
    })
}

/// A plugin's sidecar and the secrets it may receive.
struct Sidecar {
    spec: sidecar::Spec,
    names: Vec<String>,
    scope: pmt_config::ProfileScope,
    running: Mutex<Option<sidecar::Running>>,
}

impl Sidecar {
    /// Starts it (installing its runtime first when needed); the options
    /// the adapter starts with then carry its `sidecar` URL and token.
    async fn start(
        &self,
        link: Arc<dyn AdapterLink>,
        options: &str,
    ) -> Result<String, AdapterError> {
        let mut spec = self.spec.clone();
        // Only declared, granted secrets that resolve: never others.
        spec.secrets = self
            .names
            .iter()
            .filter_map(|n| {
                Some((
                    n.clone(),
                    self.scope.secrets().resolve(n).ok()?.expose().to_owned(),
                ))
            })
            .collect();
        if spec.declaration.runtime == pmt_plugins::manifest::sidecar::Runtime::Node {
            spec.node = Some(sidecar::prepare_node(&spec, &self.scope.config().network).await?);
        }
        let running = sidecar::start(&spec, link).await?;
        let mut merged: Value = serde_json::from_str(options).unwrap_or_else(|_| json!({}));
        merged["sidecar"] = running.options();
        *lock(&self.running) = Some(running);
        Ok(merged.to_string())
    }

    fn stop(&self) {
        lock(&self.running).take();
    }
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
    /// Inbound files the plugin handed over, until the gateway opens them.
    media: Arc<Inbox>,
    sidecar: Option<Sidecar>,
    /// The credential the gateway locks across homes (spec: Credential
    /// locks across homes): the values of the options the descriptor's
    /// `credential_options` names (IRC: server and nickname).
    credential: Option<String>,
}

/// The credential identity of `options` for the option keys `keys` (a
/// descriptor's `credential_options`, each `key` or `key=default`);
/// `None` when it names none or one without a default is unset.
fn credential_of(keys: &Value, options: &Value) -> Option<String> {
    let keys = keys.as_array().filter(|k| !k.is_empty())?;
    let values: Option<Vec<String>> = keys
        .iter()
        .map(|entry| {
            let (key, default) = entry.as_str()?.split_once('=').map_or_else(
                || (entry.as_str().unwrap_or_default(), None),
                |(k, d)| (k, Some(d)),
            );
            match &options[key] {
                Value::String(s) if !s.is_empty() => Some(s.to_lowercase()),
                Value::Number(n) => Some(n.to_string()),
                _ => default.map(str::to_lowercase),
            }
        })
        .collect();
    values.map(|v| v.join("|"))
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
        let credential = credential_of(&value["credential_options"], options);
        Ok(Self {
            instance,
            descriptor,
            credential,
            options: options.to_string(),
            call,
            link: Mutex::default(),
            poller: Mutex::default(),
            crashes: Arc::default(),
            media: Arc::default(),
            sidecar: None,
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

    /// Edits, deletes, reactions and typing travel through the plugin's
    /// `send` as JSON with an `op` (`edit`, `delete`, `react`, `typing`),
    /// asked only of a plugin whose descriptor declares the capability.
    fn operation(&self, declared: bool, op: Value) -> AdapterFuture<'_, Result<(), SendError>> {
        Box::pin(async move {
            if !declared {
                return Err(SendError::Unsupported);
            }
            let body = op.to_string();
            match self
                .blocking(move |instance, call| instance.adapter_send(call, &body))
                .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(why)) => Err(send_error(why)),
                Err(err) => {
                    self.crashed(&err);
                    Err(SendError::Network(err.message().to_owned()))
                }
            }
        })
    }

    fn stop_sidecar(&self) {
        if let Some(sidecar) = &self.sidecar {
            sidecar.stop();
        }
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

/// One polled message: `{id, chat, chat_type, sender, text, thread,
/// sender_name, callback, reply_to, scope, is_bot, mentioned,
/// replies_to_bot, not_addressed, attachments}` (a button press carries
/// its `callback` data; the flags say how a group message addresses the
/// bot, for the gateway's mention gating; attachments as
/// [`Inbox::attachments`] reads them).
fn inbound(value: &Value, media: &Inbox) -> Inbound {
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
    inbound.callback = text("callback");
    inbound.reply_to = text("reply_to");
    inbound.scope_id = text("scope");
    let flag = |key: &str| value[key].as_bool().unwrap_or(false);
    inbound.is_bot = flag("is_bot");
    inbound.addressing.mentioned = flag("mentioned");
    inbound.addressing.replies_to_bot = flag("replies_to_bot");
    inbound.addressing.not_addressed = flag("not_addressed");
    inbound.attachments = media.attachments(value);
    inbound
}

/// Polls until the plugin fails or traps; the gateway reconnects.
async fn poll(
    instance: Arc<Instance>,
    call: CallInfo,
    link: Arc<dyn AdapterLink>,
    crashes: Arc<Mutex<VecDeque<Instant>>>,
    media: Arc<Inbox>,
) {
    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let (plugin, call) = (Arc::clone(&instance), call.clone());
        let polled = pmt_core::spawn::blocking(move || plugin.adapter_poll(&call)).await;
        let messages = match polled {
            Ok(Ok(Ok(text))) => text,
            Ok(Ok(Err(why))) => {
                let (state, error) = adapter_media::poll_failure(why);
                link.report(state, Some(error));
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
            link.submit(inbound(value, &media)).await;
        }
    }
}

/// A send failure as the plugin classifies it by prefix (WIT `send`):
/// `rate_limited[: <retry after secs>]`, `server: ..`, `formatting: ..`,
/// `ambiguous: ..`, `permanent: ..` (never retried) or `unsupported`;
/// anything else is a network failure, retried.
fn send_error(why: String) -> SendError {
    let (kind, rest) = why
        .split_once(':')
        .map_or((why.as_str(), ""), |(kind, rest)| {
            (kind.trim(), rest.trim())
        });
    match kind {
        "rate_limited" => SendError::RateLimited {
            retry_after: rest.parse().ok().map(Duration::from_secs),
        },
        "server" => SendError::Server(rest.to_owned()),
        "formatting" => SendError::Formatting(rest.to_owned()),
        "ambiguous" => SendError::Ambiguous(rest.to_owned()),
        "permanent" => SendError::Permanent(rest.to_owned()),
        "unsupported" => SendError::Unsupported,
        _ => SendError::Network(why),
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
            let options = match &self.sidecar {
                Some(sidecar) => sidecar.start(Arc::clone(&link), &self.options).await?,
                None => self.options.clone(),
            };
            match self
                .blocking(move |instance, call| instance.adapter_start(call, &options))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(why)) => {
                    self.stop_sidecar();
                    return Err(AdapterError {
                        retryable: true,
                        code: "plugin_start".to_owned(),
                        message: why,
                    });
                }
                Err(err) => {
                    self.stop_sidecar();
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
                Arc::clone(&self.media),
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
            self.stop_sidecar();
            if let Err(err) = self.blocking(Instance::adapter_stop).await {
                tracing::warn!(plugin = %self.instance.id(), "the adapter's stop failed: {}", err.message());
            }
        })
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let mut body = json!({
            "chat": message.chat_id,
            "thread": message.thread_id,
            "reply_to": message.reply_to,
            "text": message.text,
            "buttons": message.buttons,
        });
        let attachments = message.attachments.clone();
        Box::pin(async move {
            if !attachments.is_empty() {
                body["attachments"] = Value::Array(adapter_media::outgoing(&attachments).await?);
            }
            let body = body.to_string();
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

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        let op = json!({
            "op": "edit",
            "message_id": message_id,
            "chat": message.chat_id,
            "thread": message.thread_id,
            "text": message.text,
            "buttons": message.buttons,
        });
        self.operation(self.descriptor.supports_edit, op)
    }

    fn delete<'a>(
        &'a self,
        chat_id: &'a str,
        thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        let op =
            json!({"op": "delete", "chat": chat_id, "thread": thread_id, "message_id": message_id});
        self.operation(self.descriptor.supports_delete, op)
    }

    fn react<'a>(
        &'a self,
        reaction: &'a pmt_gateway::messaging::Reaction,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        let op = json!({
            "op": "react",
            "chat": reaction.chat_id,
            "message_id": reaction.message_id,
            "emoji": reaction.emoji,
            "remove": reaction.remove,
        });
        self.operation(self.descriptor.supports_reactions, op)
    }

    fn send_typing<'a>(
        &'a self,
        chat_id: &'a str,
        thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        let op = json!({"op": "typing", "chat": chat_id, "thread": thread_id});
        self.operation(self.descriptor.supports_typing, op)
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

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move { self.media.open(attachment) })
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

    fn credential_identity(&self) -> Option<String> {
        self.credential.clone()
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
    fn the_credential_identity_comes_from_the_named_options() {
        let keys = json!(["server", "nickname=prometheus-bot"]);
        assert_eq!(
            credential_of(
                &keys,
                &json!({"server": "irc.Libera.chat", "nickname": "bot"})
            )
            .as_deref(),
            Some("irc.libera.chat|bot")
        );
        assert_eq!(
            credential_of(&keys, &json!({"server": "irc.libera.chat"})).as_deref(),
            Some("irc.libera.chat|prometheus-bot")
        );
        assert_eq!(credential_of(&keys, &json!({"nickname": "bot"})), None);
        assert_eq!(credential_of(&Value::Null, &json!({"server": "x"})), None);
    }

    #[test]
    fn the_fifth_crash_within_ten_minutes_is_fatal() {
        let crashes = Mutex::default();
        for _ in 0..4 {
            assert_eq!(record_crash(&crashes), AdapterState::Retrying);
        }
        assert_eq!(record_crash(&crashes), AdapterState::Fatal);
    }

    #[test]
    fn the_plugin_classifies_a_send_failure_by_prefix() {
        let cases = [
            (
                "permanent: chat archived",
                SendError::Permanent("chat archived".to_owned()),
            ),
            (
                "rate_limited: 7",
                SendError::RateLimited {
                    retry_after: Some(Duration::from_secs(7)),
                },
            ),
            ("rate_limited", SendError::RateLimited { retry_after: None }),
            (
                "server: status 502",
                SendError::Server("status 502".to_owned()),
            ),
            (
                "formatting: bad markup",
                SendError::Formatting("bad markup".to_owned()),
            ),
            ("unsupported", SendError::Unsupported),
            ("timed out", SendError::Network("timed out".to_owned())),
            ("status 503", SendError::Network("status 503".to_owned())),
        ];
        for (why, want) in cases {
            assert_eq!(send_error(why.to_owned()), want, "{why}");
        }
    }
}

//! The plugin event bus of one profile's load (spec: plugin-system, Plugin
//! event bus).
//!
//! `events.emit(name, payload)` publishes `<plugin_id>:<name>` (the host
//! import already refused empty names and names with `:`, so a plugin can
//! never publish into the reserved `prometheus:` namespace). Delivery is
//! asynchronous: the emitter's call returns at once and one dispatcher
//! thread delivers events in emit order (so ordered per emitter) to every
//! plugin whose `events.subscribe` pattern matches, through its
//! `extensions.on-event`. A subscriber that fails or traps is logged and
//! never affects the emitter or the other subscribers.
//!
//! Patterns are an exact name (`weather:updated`), a prefix ending in `*`
//! (`weather:*`) or `*` for every plugin event. The embedder's own events
//! port still sees every emit.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::host::ports::EventsPort;
use crate::host::{CallInfo, Instance};

/// One published event.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Published {
    name: String,
    payload: String,
}

/// What receives a plugin's event deliveries: a WASM instance, or an
/// out-of-process host such as a python-bridge sidecar.
pub trait EventSink: Send + Sync {
    /// The plugin id.
    fn id(&self) -> &str;

    /// Delivers one event (blocking).
    ///
    /// # Errors
    ///
    /// A host failure; the inner error is the plugin's own.
    fn on_event(
        &self,
        call: &CallInfo,
        name: &str,
        payload: &str,
    ) -> Result<Result<(), String>, pmt_core::error::Error>;
}

impl EventSink for Instance {
    fn id(&self) -> &str {
        Instance::id(self)
    }

    fn on_event(
        &self,
        call: &CallInfo,
        name: &str,
        payload: &str,
    ) -> Result<Result<(), String>, pmt_core::error::Error> {
        Instance::on_event(self, call, name, payload)
    }
}

#[derive(Default)]
struct Subscribers {
    /// `(plugin, pattern)` in subscription order.
    patterns: Vec<(String, String)>,
    /// The sinks that receive deliveries, once attached.
    instances: HashMap<String, Weak<dyn EventSink>>,
}

/// A profile's event bus; the [`EventsPort`] its plugins emit through.
pub struct Bus {
    profile: String,
    observer: Arc<dyn EventsPort>,
    subscribers: Arc<Mutex<Subscribers>>,
    queue: Mutex<Sender<Published>>,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bus")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl Bus {
    /// A bus for `profile` that also passes every emit to `observer` (the
    /// embedder's port), with its dispatcher thread running until the bus
    /// is dropped.
    #[must_use]
    pub fn new(profile: &str, observer: Arc<dyn EventsPort>) -> Arc<Self> {
        let (sender, receiver) = channel();
        let subscribers = Arc::new(Mutex::new(Subscribers::default()));
        let worker = Arc::clone(&subscribers);
        let owner = profile.to_owned();
        let spawned = std::thread::Builder::new()
            .name("pmt-plugin-events".to_owned())
            .spawn(move || dispatch(&owner, &worker, &receiver));
        if let Err(err) = spawned {
            tracing::error!(profile, "plugin events are not delivered: {err}");
        }
        Arc::new(Self {
            profile: profile.to_owned(),
            observer,
            subscribers,
            queue: Mutex::new(sender),
        })
    }

    /// Delivers the subscriptions of `instance` to it from now on.
    pub fn attach(&self, instance: &Arc<Instance>) {
        let sink: Arc<dyn EventSink> = Arc::clone(instance) as _;
        self.attach_sink(&sink);
    }

    /// Delivers the subscriptions of `sink`'s plugin to it from now on (the
    /// bus keeps it weakly: its owner keeps it alive).
    pub fn attach_sink(&self, sink: &Arc<dyn EventSink>) {
        lock(&self.subscribers)
            .instances
            .insert(sink.id().to_owned(), Arc::downgrade(sink));
    }

    /// The patterns `plugin` subscribed to.
    #[must_use]
    pub fn subscriptions(&self, plugin: &str) -> Vec<String> {
        lock(&self.subscribers)
            .patterns
            .iter()
            .filter(|(p, _)| p == plugin)
            .map(|(_, pattern)| pattern.clone())
            .collect()
    }
}

impl EventsPort for Bus {
    fn emit(&self, profile: &str, name: &str, payload: &str) {
        self.observer.emit(profile, name, payload);
        let event = Published {
            name: name.to_owned(),
            payload: payload.to_owned(),
        };
        if lock(&self.queue).send(event).is_err() {
            tracing::debug!(event = name, "plugin event dropped: the bus stopped");
        }
    }

    fn subscribe(&self, profile: &str, plugin: &str, pattern: &str) {
        self.observer.subscribe(profile, plugin, pattern);
        let mut subscribers = lock(&self.subscribers);
        let entry = (plugin.to_owned(), pattern.to_owned());
        if !subscribers.patterns.contains(&entry) {
            subscribers.patterns.push(entry);
        }
    }
}

/// Whether `pattern` selects the event `name`.
#[must_use]
pub fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn dispatch(profile: &str, subscribers: &Mutex<Subscribers>, queue: &Receiver<Published>) {
    for event in queue {
        for instance in recipients(subscribers, &event.name) {
            let call = CallInfo {
                surface: "events".to_owned(),
                ..CallInfo::default()
            };
            let failure = match instance.on_event(&call, &event.name, &event.payload) {
                Ok(Ok(())) => None,
                Ok(Err(message)) => Some(message),
                Err(err) => Some(err.message().to_owned()),
            };
            if let Some(message) = failure {
                tracing::warn!(
                    profile,
                    plugin = instance.id(),
                    event = %event.name,
                    "event subscriber failed: {message}"
                );
            }
        }
    }
}

/// The live, attached instances subscribed to `name`, once each, in
/// subscription order.
fn recipients(subscribers: &Mutex<Subscribers>, name: &str) -> Vec<Arc<dyn EventSink>> {
    let subscribers = lock(subscribers);
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for (plugin, pattern) in &subscribers.patterns {
        if seen.contains(plugin) || !matches(pattern, name) {
            continue;
        }
        if let Some(instance) = subscribers.instances.get(plugin).and_then(Weak::upgrade) {
            seen.push(plugin.clone());
            out.push(instance);
        }
    }
    out
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn patterns_are_exact_names_or_prefixes() {
        assert!(matches("weather:updated", "weather:updated"));
        assert!(!matches("weather:updated", "weather:updated2"));
        assert!(matches("weather:*", "weather:updated"));
        assert!(!matches("weather:*", "notes:updated"));
        assert!(matches("*", "notes:updated"));
    }
}

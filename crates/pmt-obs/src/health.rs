//! The component health model (spec: observability, Component health
//! model; Health endpoints): every daemon component (each profile's store,
//! the scheduler, each adapter, each MCP server, each plugin host, the
//! Python bridge, the OTLP exporter, each provider's circuit) reports
//! `ok`, `degraded` or `down` with a reason and the time it entered that
//! state. A transition is logged (WARN, or INFO back to `ok`) and handed
//! to every subscriber (the daemon turns it into `health.changed`).
//!
//! Components are keyed by `(profile, name)`: `name` is `<kind>` or
//! `<kind>:<instance>` (`store`, `scheduler`, `adapter:telegram`,
//! `mcp:github`, `provider:openai`); daemon-wide components have no
//! profile. `/readyz` reports only kinds, never profile names.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

/// A component's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Working.
    Ok,
    /// Working with trouble (retrying, partial failures).
    Degraded,
    /// Not working.
    Down,
}

impl State {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Down => "down",
        }
    }
}

/// One component's health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// `<kind>` or `<kind>:<instance>`.
    pub name: String,
    /// The owning profile; `None` for daemon-wide components.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Its state.
    pub state: State,
    /// Why (empty when `ok` for no particular reason).
    pub reason: String,
    /// When it entered the state (RFC 3339).
    pub since: String,
}

impl Component {
    /// The kind (`adapter` of `adapter:telegram`).
    #[must_use]
    pub fn kind(&self) -> &str {
        self.name.split(':').next().unwrap_or(&self.name)
    }
}

/// A transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The component after the transition.
    pub component: Component,
    /// Its previous state (`None` when first reported).
    pub previous: Option<State>,
}

type Subscriber = Arc<dyn Fn(&Change) + Send + Sync>;

/// The components of one process.
#[derive(Default)]
pub struct Health {
    components: Mutex<BTreeMap<(Option<String>, String), Component>>,
    subscribers: Mutex<Vec<Subscriber>>,
}

impl std::fmt::Debug for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Health").finish_non_exhaustive()
    }
}

/// The process-wide registry.
#[must_use]
pub fn global() -> &'static Health {
    static GLOBAL: OnceLock<Health> = OnceLock::new();
    GLOBAL.get_or_init(Health::default)
}

impl Health {
    /// Records `name`'s state; a change of state is logged and handed to
    /// the subscribers (a new reason in the same state only updates it).
    pub fn report(&self, profile: Option<&str>, name: &str, state: State, reason: &str) {
        let key = (profile.map(str::to_owned), name.to_owned());
        let change = {
            let mut components = self
                .components
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let previous = components.get(&key).map(|c| c.state);
            if previous == Some(state) {
                if let Some(c) = components.get_mut(&key) {
                    reason.clone_into(&mut c.reason);
                }
                return;
            }
            let component = Component {
                name: name.to_owned(),
                profile: key.0.clone(),
                state,
                reason: reason.to_owned(),
                since: crate::trajectory::timestamp(jiff::Timestamp::now()),
            };
            components.insert(key, component.clone());
            Change {
                component,
                previous,
            }
        };
        log(&change);
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for subscriber in subscribers {
            subscriber(&change);
        }
    }

    /// Forgets `name` (a removed adapter or server).
    pub fn remove(&self, profile: Option<&str>, name: &str) {
        self.components
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(profile.map(str::to_owned), name.to_owned()));
    }

    /// Calls `subscriber` on every transition from now on.
    pub fn subscribe(&self, subscriber: impl Fn(&Change) + Send + Sync + 'static) {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::new(subscriber));
    }

    /// Every component of `profile` plus the daemon-wide ones (`None`:
    /// every component).
    #[must_use]
    pub fn components(&self, profile: Option<&str>) -> Vec<Component> {
        self.components
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|c| profile.is_none() || c.profile.is_none() || c.profile.as_deref() == profile)
            .cloned()
            .collect()
    }

    /// The kinds among `kinds` with a component that is not `ok`
    /// (`/readyz`), sorted, each once.
    #[must_use]
    pub fn failing(&self, kinds: &[&str]) -> Vec<String> {
        let mut failing: Vec<String> = self
            .components(None)
            .iter()
            .filter(|c| c.state != State::Ok && kinds.contains(&c.kind()))
            .map(|c| c.kind().to_owned())
            .collect();
        failing.sort();
        failing.dedup();
        failing
    }
}

fn log(change: &Change) {
    let c = &change.component;
    let profile = c.profile.as_deref().unwrap_or("-");
    if c.state == State::Ok {
        tracing::info!(
            component_name = c.name,
            health = "ok",
            "{} ({profile}) is ok again",
            c.name
        );
    } else {
        tracing::warn!(
            component_name = c.name,
            health = c.state.as_str(),
            "{} ({profile}) is {}: {}",
            c.name,
            c.state.as_str(),
            c.reason
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn only_transitions_reach_subscribers_and_failing_names_kinds() {
        let health = Health::default();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        health.subscribe(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        health.report(Some("work"), "store", State::Down, "migrating");
        health.report(Some("work"), "store", State::Down, "still migrating");
        health.report(None, "scheduler", State::Ok, "");
        assert_eq!(seen.load(Ordering::Relaxed), 2);
        assert_eq!(health.failing(&["store", "scheduler"]), ["store"]);
        health.report(Some("work"), "store", State::Ok, "");
        assert!(health.failing(&["store", "scheduler"]).is_empty());
        assert_eq!(health.components(Some("other")).len(), 1);
    }
}

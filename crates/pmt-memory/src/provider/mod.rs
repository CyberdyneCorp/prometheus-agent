//! The external memory provider of a session (spec: memory, External
//! memory provider contract; One external provider, isolated and bounded;
//! Provider egress redaction and write mirroring; Profile-bound providers;
//! Pre-compress checkpoint; External provider recall budget and spill).
//!
//! A profile runs the built-in store plus at most one provider. The
//! provider itself (a WASM plugin, or anything else) sits behind
//! [`ProviderBackend`]; this module is the host side every provider gets:
//!
//! - isolation: a failing call is logged and treated as a no-op, never an
//!   error of the turn or of the built-in store;
//! - bounds: `prefetch` gets `memory.provider_prefetch_timeout_ms` and the
//!   turn goes on without its recall after that; `sync-turn`,
//!   `queue-prefetch`, `on-turn-start` and `on-memory-write` run on one
//!   serialized background queue per session ([`queue`]), never inline,
//!   and closing the session drains that queue for at most 5000 ms;
//! - egress: everything sent passes secret redaction (the stored
//!   transcript keeps the original) with skill-invocation scaffolding
//!   stripped ([`egress`]);
//! - profile binding: the session is bound to one profile at start, and a
//!   call made for another profile is refused and logged;
//! - recall budget: the provider's recall is bounded separately from the
//!   built-in recall ([`spill`]).

pub mod egress;
pub mod queue;
pub mod spill;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::secret::Redactor;

use self::queue::{Job, Queue};

/// The error text a backend's `prefetch` starts with when its bound cut
/// the call.
pub const TIMED_OUT: &str = "timed out";

/// How long closing a session waits for its queue.
pub const DRAIN: Duration = Duration::from_secs(5);

/// The session a provider serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// The session id.
    pub session_id: String,
    /// The profile it belongs to.
    pub profile: String,
    /// `cli`, `tui`, `telegram`...
    pub platform: String,
    /// The caller identity that scopes long-term memory across sessions
    /// (an API caller's session key), when there is one.
    pub user_id: Option<String>,
}

/// A tool a provider offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTool {
    /// Its name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// Its JSON Schema (text).
    pub parameters: String,
}

/// One setting `prometheus memory setup` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupField {
    /// The key (`memory.providers.<id>.<key>`, or the secret's name).
    pub key: String,
    /// What it is.
    pub description: String,
    /// Written to the secrets store, never to `config.toml`.
    pub secret: bool,
    /// Setup refuses an empty answer.
    pub required: bool,
    /// The value an empty answer takes.
    pub default: Option<String>,
}

/// One committed built-in memory operation, mirrored to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirrored {
    /// `add`, `patch`, `remove`, `update`.
    pub action: String,
    /// `memory` or `user`.
    pub target: String,
    /// The entry.
    pub entry_id: String,
    /// Its content after the operation (empty for a removal).
    pub content: String,
    /// Its content before, from the committed revision.
    pub previous: Option<String>,
}

/// A provider as the host drives it. Every call may fail; the host logs a
/// failure and goes on. Calls block (a WASM call): the host runs them off
/// the async threads.
pub trait ProviderBackend: Send + Sync {
    /// The plugin id (`memory.provider`).
    fn id(&self) -> &str;
    /// `name`.
    fn name(&self) -> String;
    /// `is-available` (no network).
    fn is_available(&self) -> bool;
    /// `unavailable-reason`.
    fn unavailable_reason(&self) -> Option<String>;
    /// `initialize` (or `on-session-switch` when the instance already
    /// served another session).
    ///
    /// # Errors
    ///
    /// The provider's refusal.
    fn initialize(&self, session: &SessionInfo) -> Result<(), String>;
    /// `tool-schemas`.
    fn tool_schemas(&self) -> Vec<ProviderTool>;
    /// `system-prompt-block`.
    fn prompt_block(&self) -> Option<String>;
    /// `prefetch`, within `bound`.
    ///
    /// # Errors
    ///
    /// The provider's failure, a trap or the timeout.
    fn prefetch(&self, query: &str, bound: Duration) -> Result<Option<String>, String>;
    /// `queue-prefetch`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn queue_prefetch(&self, query: &str) -> Result<(), String>;
    /// `sync-turn`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn sync_turn(&self, user: &str, assistant: &str) -> Result<(), String>;
    /// `handle-tool-call`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`]; the tool's own error.
    fn tool_call(&self, name: &str, arguments: &str) -> Result<String, String>;
    /// `on-turn-start`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn turn_start(&self, text: &str) -> Result<(), String>;
    /// `on-session-end`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn session_end(&self) -> Result<(), String>;
    /// `on-pre-compress`: whether the checkpoint was confirmed.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn pre_compress(&self, messages: &str) -> Result<bool, String>;
    /// `on-delegation`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn delegation(&self, task: &str) -> Result<(), String>;
    /// `on-memory-write`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn memory_write(&self, write: &Mirrored) -> Result<(), String>;
    /// `config-schema`.
    fn config_schema(&self) -> Vec<SetupField>;
    /// `post-setup`.
    ///
    /// # Errors
    ///
    /// As [`Self::prefetch`].
    fn post_setup(&self) -> Result<(), String>;
}

/// The message of a call refused for another profile.
#[must_use]
pub fn mismatch(name: &str) -> String {
    format!("Memory provider '{name}' refused: profile scope mismatch")
}

/// What a provider session is started with.
pub struct ProviderStart {
    /// The provider.
    pub backend: Arc<dyn ProviderBackend>,
    /// The profile it is bound to.
    pub scope: ProfileScope,
    /// The session.
    pub session: SessionInfo,
    /// The profile's secrets (egress redaction).
    pub redactor: Redactor,
}

/// The provider of one session, bound to its profile.
pub struct ProviderSession {
    backend: Arc<dyn ProviderBackend>,
    name: String,
    profile: String,
    session: String,
    redactor: Redactor,
    prefetch_timeout: Duration,
    recall_max_chars: usize,
    spill_dir: Option<PathBuf>,
    prompt_block: Option<String>,
    queue: Queue,
}

impl std::fmt::Debug for ProviderSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSession")
            .field("provider", &self.backend.id())
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl ProviderSession {
    /// Initializes the provider for `start.session`, bound to
    /// `start.scope`; must run inside a tokio runtime (the queue's worker).
    ///
    /// # Errors
    ///
    /// The provider's refusal of `initialize`: the session then runs with
    /// built-in memory only.
    pub async fn start(start: ProviderStart) -> Result<Self, String> {
        let ProviderStart {
            backend,
            scope,
            session,
            redactor,
        } = start;
        let init = Arc::clone(&backend);
        let info = session.clone();
        let prompt_block =
            pmt_core::spawn::blocking(move || init.initialize(&info).map(|()| init.prompt_block()))
                .await
                .map_err(|e| e.to_string())??;
        let config = &scope.config().memory;
        let name = backend.name();
        let spill_dir = config
            .provider_recall_spill
            .then(|| scope.dir().join("cache").join("memory-recall"));
        Ok(Self {
            queue: Queue::start(Arc::clone(&backend), name.clone()),
            backend,
            name,
            profile: session.profile,
            session: session.session_id,
            redactor,
            prefetch_timeout: Duration::from_millis(config.provider_prefetch_timeout_ms),
            recall_max_chars: usize::try_from(config.provider_recall_max_chars)
                .unwrap_or(usize::MAX),
            spill_dir,
            prompt_block: prompt_block.filter(|b| !b.trim().is_empty()),
        })
    }

    /// The provider's `system-prompt-block`, read once at start.
    #[must_use]
    pub fn prompt_block(&self) -> Option<&str> {
        self.prompt_block.as_deref()
    }

    /// The provider's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The provider.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn ProviderBackend> {
        &self.backend
    }

    /// The profile the session is bound to.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Whether a call for `profile` may reach this provider; a mismatch is
    /// logged.
    #[must_use]
    pub fn bound_to(&self, profile: &str) -> bool {
        let ok = profile == self.profile;
        if !ok {
            tracing::warn!(
                bound = %self.profile,
                caller = %profile,
                "{}",
                mismatch(&self.name)
            );
        }
        ok
    }

    fn egress(&self, text: &str) -> String {
        egress::outbound(&self.redactor, text)
    }

    /// This turn's provider recall (already bounded by the recall budget),
    /// or `None` (no recall, a failure, or the timeout: logged).
    pub async fn prefetch(&self, profile: &str, text: &str) -> Option<String> {
        if !self.bound_to(profile) {
            return None;
        }
        let query = self.egress(text);
        self.queue.push(Job::TurnStart(query.clone()));
        let backend = Arc::clone(&self.backend);
        let bound = self.prefetch_timeout;
        let call = pmt_core::spawn::blocking(move || backend.prefetch(&query, bound));
        // The plugin call is cut at `bound`; the grace covers the hop.
        let recall = match tokio::time::timeout(bound + Duration::from_millis(250), call).await {
            Ok(Ok(Ok(recall))) => recall?,
            Ok(Ok(Err(err))) if !err.starts_with(TIMED_OUT) => {
                tracing::warn!(provider = %self.name, "memory provider prefetch failed: {err}");
                return None;
            }
            Ok(Err(err)) => {
                tracing::warn!(provider = %self.name, "memory provider prefetch panicked: {err}");
                return None;
            }
            Ok(Ok(Err(_))) | Err(_) => {
                tracing::warn!(
                    provider = %self.name,
                    timeout_ms = bound.as_millis(),
                    "memory provider prefetch timed out; the turn goes on without its recall"
                );
                return None;
            }
        };
        if recall.trim().is_empty() {
            return None;
        }
        Some(spill::bounded(
            &recall,
            self.recall_max_chars,
            self.spill_dir.as_deref(),
            &self.session,
        ))
    }

    /// A completed turn, synced in the background.
    pub fn sync_turn(&self, profile: &str, user: &str, assistant: &str) {
        if self.bound_to(profile) {
            self.queue.push(Job::Sync {
                user: self.egress(user),
                assistant: self.egress(assistant),
            });
            self.queue.push(Job::QueuePrefetch(self.egress(user)));
        }
    }

    /// Committed built-in writes, mirrored in the background.
    pub fn mirror(&self, profile: &str, writes: Vec<Mirrored>) {
        if !self.bound_to(profile) {
            return;
        }
        for mut write in writes {
            write.content = self.egress(&write.content);
            write.previous = write.previous.map(|p| self.egress(&p));
            self.queue.push(Job::Write(write));
        }
    }

    /// A delegated task, told in the background.
    pub fn delegation(&self, profile: &str, task: &str) {
        if self.bound_to(profile) {
            self.queue.push(Job::Delegation(self.egress(task)));
        }
    }

    /// The pre-compress checkpoint over `messages` (a JSON list, redacted
    /// here): `true` when the provider confirmed it.
    pub async fn pre_compress(&self, profile: &str, messages: &str) -> bool {
        if !self.bound_to(profile) {
            return false;
        }
        let messages = self.egress(messages);
        let backend = Arc::clone(&self.backend);
        match pmt_core::spawn::blocking(move || backend.pre_compress(&messages)).await {
            Ok(Ok(confirmed)) => confirmed,
            Ok(Err(err)) => {
                tracing::warn!(provider = %self.name, "memory provider checkpoint failed: {err}");
                false
            }
            Err(err) => {
                tracing::warn!(provider = %self.name, "memory provider checkpoint panicked: {err}");
                false
            }
        }
    }

    /// One call of a provider tool.
    ///
    /// # Errors
    ///
    /// The provider's error, or the profile mismatch.
    pub async fn tool_call(
        &self,
        profile: &str,
        name: &str,
        arguments: &str,
    ) -> Result<String, String> {
        if !self.bound_to(profile) {
            return Err(mismatch(&self.name));
        }
        let (backend, name, arguments) = (
            Arc::clone(&self.backend),
            name.to_owned(),
            self.egress(arguments),
        );
        pmt_core::spawn::blocking(move || backend.tool_call(&name, &arguments))
            .await
            .map_err(|e| e.to_string())?
    }

    /// Ends the session: drains the queue for at most [`DRAIN`], then
    /// `on-session-end`.
    pub async fn close(&self) {
        self.queue.drain(DRAIN).await;
        let backend = Arc::clone(&self.backend);
        if let Ok(Err(err)) = pmt_core::spawn::blocking(move || backend.session_end()).await {
            tracing::warn!(provider = %self.name, "memory provider on-session-end failed: {err}");
        }
    }
}

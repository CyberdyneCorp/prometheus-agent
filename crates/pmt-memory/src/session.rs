//! One session's memory: the `memory` tool, per-turn recall and the
//! review nudge (spec: memory, Memory tool surface; Per-turn retry cap;
//! Write approval and pending changes; Background review cannot delete
//! unattended; Memory nudge interval; Per-turn recall injection).

use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;
use pmt_agent::tools::{MemoryPort, Recalled};
use pmt_agent::turn::PromptSetup;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_config::schema::{MemoryConfig, MemoryReviewMode};
use pmt_core::id::SessionId;
use pmt_core::secret::Redactor;
use pmt_store::{MemoryOrigin, Source, Store};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use crate::ops::{self, Action, Call, Op};
use crate::provider::{Mirrored, ProviderSession};
use crate::rank::Embedder;
use crate::review::Nudge;
use crate::write::{self, Written};
use crate::{budget, recall, scan, snapshot};

/// Failed calls in one turn after which the tool tells the model to stop.
pub const RETRY_CAP: u32 = 3;

/// What the model is told once [`RETRY_CAP`] is passed.
pub const STOP_RETRYING: &str =
    "Stop retrying memory writes in this turn and continue your reply to the user.";

/// Asks the person at an interactive foreground surface to approve a
/// memory change.
pub trait Approver: Send + Sync {
    /// Shows `preview`; `Some(answer)`, or `None` when nobody answered (the
    /// change is then staged).
    fn ask<'a>(&'a self, preview: &'a str) -> BoxFuture<'a, Option<bool>>;
}

/// What a [`MemorySession`] is built from.
pub struct MemorySetup {
    /// The profile.
    pub scope: ProfileScope,
    /// The profile's store.
    pub store: Arc<Store>,
    /// The session whose writes these are (recorded on entries).
    pub session: Option<SessionId>,
    /// The session's source (cron, subagent and harness sessions never
    /// run the review nudge).
    pub source: Source,
    /// A background review: adds commit, patch/remove/unpin are staged.
    pub review: bool,
    /// Inline approval on an interactive foreground CLI session.
    pub approver: Option<Arc<dyn Approver>>,
    /// Known secret values (the profile's store) for the secret check.
    pub redactor: Redactor,
    /// User turns the session already had (a resumed session).
    pub prior_user_turns: u32,
    /// Embeddings for hybrid recall (`memory.recall.embedding_model`).
    pub embedder: Option<Arc<dyn Embedder>>,
    /// The profile's external memory provider, started for this session.
    pub provider: Option<Arc<ProviderSession>>,
}

impl MemorySetup {
    /// A setup for `session` from `source`: attended, no inline approver.
    #[must_use]
    pub fn new(
        scope: ProfileScope,
        store: Arc<Store>,
        session: Option<SessionId>,
        source: Source,
    ) -> Self {
        Self {
            scope,
            store,
            session,
            source,
            review: false,
            approver: None,
            redactor: Redactor::default(),
            prior_user_turns: 0,
            embedder: None,
            provider: None,
        }
    }
}

#[derive(Debug, Default)]
struct TurnState {
    failures: u32,
    called: bool,
}

/// One session's memory.
pub struct MemorySession {
    setup: MemorySetup,
    turn: Mutex<TurnState>,
    nudge: Mutex<Nudge>,
}

impl MemorySession {
    /// The memory of `setup`'s session.
    #[must_use]
    pub fn new(setup: MemorySetup) -> Self {
        let nudge = Nudge::new(
            &setup.scope.config().memory,
            setup.source,
            setup.prior_user_turns,
        );
        Self {
            setup,
            turn: Mutex::new(TurnState::default()),
            nudge: Mutex::new(nudge),
        }
    }

    /// The profile's memory settings (fixed for the session).
    #[must_use]
    pub fn config(&self) -> &MemoryConfig {
        &self.setup.scope.config().memory
    }

    /// The setup.
    #[must_use]
    pub fn setup(&self) -> &MemorySetup {
        &self.setup
    }

    /// Whether the session offers the `memory` tool.
    #[must_use]
    pub fn available(&self) -> bool {
        !budget::enabled(self.config()).is_empty()
    }

    /// The memory section of a new prompt (`PromptSetup::memory` at session
    /// start). A store error is logged and leaves the section out.
    #[must_use]
    pub fn snapshot(&self) -> Option<String> {
        let builtin = snapshot::render(&self.setup.store, self.config()).unwrap_or_else(|err| {
            tracing::warn!("memory snapshot skipped: {err}");
            None
        });
        // The provider's block follows the built-in blocks, read once at
        // the provider's start so it freezes with the prompt.
        let block = self.setup.provider.as_ref().and_then(|p| p.prompt_block());
        match (builtin, block) {
            (Some(builtin), Some(block)) => Some(format!("{builtin}\n\n{block}")),
            (None, Some(block)) => Some(block.to_owned()),
            (builtin, None) => builtin,
        }
    }

    /// Re-reads the snapshot into `prompt` (the session's `refresh_prompt`
    /// calls this before every rebuild: compaction, `/memory reload --now`).
    pub fn refresh(&self, prompt: &mut PromptSetup) {
        prompt.memory = self.snapshot();
    }

    /// Ends a user turn whose reply was delivered: `true` when a
    /// background memory review is due now (the caller runs
    /// [`crate::review::run`]).
    pub fn end_turn(&self) -> bool {
        let called = std::mem::take(&mut self.lock_turn().called);
        self.nudge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(called, self.available())
    }

    fn lock_turn(&self) -> std::sync::MutexGuard<'_, TurnState> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn origin(&self) -> MemoryOrigin {
        if self.setup.review {
            MemoryOrigin::BackgroundReview
        } else {
            MemoryOrigin::Agent
        }
    }

    fn approval_required(&self) -> bool {
        let config = self.config();
        config.write_approval || config.review_mode == MemoryReviewMode::Require
    }

    /// Runs one validated, scanned call; `Err` is the refusal text.
    async fn execute(&self, arguments: &Value) -> Result<Value, String> {
        let config = self.config();
        let call = ops::parse(arguments, &budget::enabled(config), Timestamp::now())?;
        for (i, op) in call.ops.iter().enumerate() {
            if let Some(content) = &op.content {
                scan::check(content, &self.setup.redactor)
                    .map_err(|m| format!("operations[{i}] {m}"))?;
            }
        }
        let (commit, stage) = self.split(&call).await?;
        let ctx = write::context(
            config,
            call.target,
            self.origin(),
            self.setup.session.map(|s| s.to_string()),
        );
        let last = self.last_revision();
        let written = write::write(&self.setup.store, &ctx, &commit, &stage)
            .map_err(|e| e.message().to_owned())?;
        self.committed(last);
        Ok(reply(call.target.as_str(), &written))
    }

    /// This session's newest memory revision.
    fn last_revision(&self) -> Option<i64> {
        let session = self.setup.session?.to_string();
        let filter = pmt_store::RevisionFilter {
            session: Some(session),
            ..pmt_store::RevisionFilter::default()
        };
        self.setup
            .store
            .memory_revisions(&filter)
            .ok()?
            .last()
            .map(|r| r.rev)
    }

    /// After a write: the revisions it committed (after `last`) reach the
    /// external provider (staged changes are not revisions yet).
    fn committed(&self, last: Option<i64>) {
        let (Some(provider), Some(session)) = (&self.setup.provider, self.setup.session) else {
            return;
        };
        let filter = pmt_store::RevisionFilter {
            session: Some(session.to_string()),
            after_rev: Some(last.unwrap_or(0)),
            ..pmt_store::RevisionFilter::default()
        };
        let revisions = match self.setup.store.memory_revisions(&filter) {
            Ok(revisions) => revisions,
            Err(err) => {
                tracing::warn!("memory writes not mirrored to the provider: {err}");
                return;
            }
        };
        let writes = revisions.iter().map(mirrored).collect();
        provider.mirror(self.setup.scope.name().as_str(), writes);
    }

    /// Which operations commit now and which are staged for approval.
    async fn split(&self, call: &Call) -> Result<(Vec<Op>, Vec<Op>), String> {
        if self.setup.review {
            if self.approval_required() {
                return Ok((Vec::new(), call.ops.clone()));
            }
            let (stage, commit): (Vec<Op>, Vec<Op>) = call.ops.iter().cloned().partition(|op| {
                matches!(op.action, Action::Patch | Action::Remove | Action::Unpin)
            });
            return Ok((commit, stage));
        }
        if !self.approval_required() {
            return Ok((call.ops.clone(), Vec::new()));
        }
        let answer = match &self.setup.approver {
            Some(approver) => approver.ask(&preview(call)).await,
            None => None,
        };
        match answer {
            Some(true) => Ok((call.ops.clone(), Vec::new())),
            Some(false) => Err("the user declined this memory change; do not retry it".to_owned()),
            None => Ok((Vec::new(), call.ops.clone())),
        }
    }
}

/// The provider's view of a committed revision.
fn mirrored(revision: &pmt_store::Revision) -> Mirrored {
    Mirrored {
        action: revision.operation.as_str().to_owned(),
        target: revision.target.as_str().to_owned(),
        entry_id: revision.entry_id.clone(),
        content: revision
            .after
            .as_ref()
            .map(|e| e.content.clone())
            .unwrap_or_default(),
        previous: revision.before.as_ref().map(|e| e.content.clone()),
    }
}

/// The approval prompt for `call`.
fn preview(call: &Call) -> String {
    let lines: Vec<String> = call
        .ops
        .iter()
        .map(|op| {
            let what = op
                .id
                .as_deref()
                .or(op.old_text.as_deref())
                .unwrap_or_default();
            match &op.content {
                Some(content) => format!("  {} {what} {content}", op.action.as_str()),
                None => format!("  {} {what}", op.action.as_str()),
            }
        })
        .collect();
    format!("Save to {} memory?\n{}", call.target, lines.join("\n"))
}

/// The tool result of a successful call.
fn reply(target: &str, written: &Written) -> Value {
    let mut out = json!({
        "success": true,
        "target": target,
        "results": written.results,
        "usage": written.usage,
    });
    if let Some(id) = &written.pending_id {
        out["proposal_staged"] = json!(true);
        out["pending_id"] = json!(id);
        out["staged"] = json!(written.staged);
        out["message"] = json!(format!(
            "Staged for the user's review: they approve it with /memory approve {id} (see /memory pending)."
        ));
    } else if !written.results.is_empty() {
        out["message"] =
            json!("Saved. Core entries reach the system prompt from the next session.");
    }
    out
}

impl MemoryPort for MemorySession {
    fn begin_turn<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Option<Recalled>> {
        *self.lock_turn() = TurnState::default();
        Box::pin(async move {
            let setup = &self.setup;
            let builtin = async {
                recall::recall(
                    &setup.store,
                    self.config(),
                    setup.embedder.as_deref(),
                    &setup.redactor,
                    text,
                )
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!("memory recall skipped: {err}");
                    None
                })
            };
            let external = async {
                match &setup.provider {
                    Some(provider) if !recall::is_trivial(text) => {
                        provider.prefetch(setup.scope.name().as_str(), text).await
                    }
                    _ => None,
                }
            };
            let (builtin, external) = tokio::join!(builtin, external);
            recall::merge(builtin, external)
        })
    }

    fn turn_completed(&self, text: &str, reply: &str) {
        if let Some(provider) = &self.setup.provider {
            provider.sync_turn(self.setup.scope.name().as_str(), text, reply);
        }
    }

    fn checkpoint<'a>(&'a self, messages: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            let provider = self.setup.provider.as_ref()?;
            Some(
                provider
                    .pre_compress(self.setup.scope.name().as_str(), messages)
                    .await,
            )
        })
    }

    fn call(&self, arguments: Value) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            self.lock_turn().called = true;
            match self.execute(&arguments).await {
                Ok(value) => {
                    self.lock_turn().failures = 0;
                    ToolResult::ok(value.to_string())
                }
                Err(message) => {
                    let failures = {
                        let mut turn = self.lock_turn();
                        turn.failures += 1;
                        turn.failures
                    };
                    let mut out = json!({"success": false, "error": message});
                    if failures > RETRY_CAP {
                        out["done"] = json!(true);
                        out["message"] = json!(STOP_RETRYING);
                    }
                    ToolResult::error(out.to_string())
                }
            }
        })
    }
}

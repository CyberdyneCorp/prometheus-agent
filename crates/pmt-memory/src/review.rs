//! The memory nudge and the background memory review (spec: memory,
//! Memory nudge interval; Background review cannot delete unattended).
//!
//! [`Nudge`] counts user turns since the last `memory` call; at
//! `memory.nudge_interval` the session runs [`run`] after delivering the
//! reply. The review is a hidden child session whose only tool is
//! `memory` in review mode: its adds commit, its patches, removals and
//! unpins become pending proposals.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use jiff::Timestamp;

use pmt_agent::review::excerpt;
use pmt_agent::tools::MemoryPort;
use pmt_agent::turn::model::{BoxFuture, ModelPort, RequestedCall};
use pmt_agent::turn::tools::{ToolBatch, ToolOutcome, ToolPort};
use pmt_agent::turn::{Agent, AgentSetup, PromptSetup, TurnInput, TurnOptions, TurnResult};
use pmt_config::schema::MemoryConfig;
use pmt_core::cancel::CancelToken;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_providers::request::Tool;
use pmt_store::{Source, Store};
use pmt_tools::ToolResult;

use crate::session::{MemorySession, MemorySetup};
use crate::tool;

/// Counts user turns towards a memory review.
#[derive(Debug, Clone)]
pub struct Nudge {
    interval: u32,
    count: u32,
    eligible: bool,
}

impl Nudge {
    /// The counter of a session from `source` that already had
    /// `prior_user_turns` (a resumed session restores
    /// `prior_user_turns % interval`); cron, delegated children and harness
    /// evolution never review.
    #[must_use]
    pub fn new(config: &MemoryConfig, source: Source, prior_user_turns: u32) -> Self {
        let interval = config.nudge_interval;
        Self {
            interval,
            count: prior_user_turns.checked_rem(interval).unwrap_or(0),
            eligible: !matches!(source, Source::Cron | Source::Subagent | Source::Harness),
        }
    }

    /// Adds a finished user turn (`memory_called`: the turn called the
    /// tool); `true` when a review is due now (the counter restarts).
    pub fn observe(&mut self, memory_called: bool, tool_available: bool) -> bool {
        if memory_called {
            self.count = 0;
            return false;
        }
        if !self.eligible || self.interval == 0 || !tool_available {
            return false;
        }
        self.count += 1;
        if self.count >= self.interval {
            self.count = 0;
            return true;
        }
        false
    }

    /// User turns since the last memory call or review.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }
}

/// The review instruction.
pub const REVIEW_PROMPT: &str = "Review the conversation below for durable knowledge worth \
remembering across sessions: who the user is and how they like to work (target \"user\"), and \
facts about their environment and projects or lessons learned (target \"memory\"). Save new \
knowledge with the memory tool in one batched call per target; propose corrections to entries \
that turned out wrong. Skip anything temporary, task progress and secrets. If nothing is worth \
saving, answer \"nothing to save\" without calling tools.";

/// The review's only tool: `memory`, run by a review-mode session.
struct ReviewTools {
    memory: Arc<MemorySession>,
    definition: Tool,
}

impl ToolPort for ReviewTools {
    fn definitions(&self) -> Vec<Tool> {
        vec![self.definition.clone()]
    }

    fn run<'a>(
        &'a self,
        calls: &'a [RequestedCall],
        _: &'a CancelToken,
    ) -> BoxFuture<'a, ToolBatch> {
        Box::pin(async move {
            let mut outcomes = Vec::with_capacity(calls.len());
            for call in calls {
                let result = if call.name == tool::NAME {
                    match serde_json::from_str(&call.arguments) {
                        Ok(arguments) => self.memory.call(arguments).await,
                        Err(e) => ToolResult::error(format!("arguments are not JSON: {e}")),
                    }
                } else {
                    ToolResult::error(format!(
                        "unknown tool '{}': only memory is available",
                        call.name
                    ))
                };
                outcomes.push(ToolOutcome {
                    result,
                    duration_ms: 0,
                });
            }
            ToolBatch {
                outcomes,
                plan: None,
            }
        })
    }
}

/// What a background memory review runs with.
pub struct ReviewRun {
    /// The reviewed session's memory.
    pub parent: Arc<MemorySession>,
    /// The reviewed session.
    pub session: SessionId,
    /// The review's hidden child session (`pmt_agent::review::child_session`).
    pub child: SessionId,
    /// The model (normally the session's main route).
    pub model: Arc<dyn ModelPort>,
    /// What the user asked the review to look for (`/refine <focus>`).
    pub focus: Option<String>,
}

/// The review's user message: [`REVIEW_PROMPT`], the focus, the
/// conversation.
#[must_use]
pub fn instruction(focus: Option<&str>, conversation: &str) -> String {
    let mut text = REVIEW_PROMPT.to_owned();
    if let Some(focus) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        let _ = write!(text, "\n\nThe user asked this review to focus on: {focus}");
    }
    let _ = write!(text, "\n\n<conversation>\n{conversation}\n</conversation>");
    text
}

/// The memory state a review is measured against ([`Mark::changes`]).
#[derive(Debug, Clone)]
pub struct Mark {
    entries: BTreeMap<String, Timestamp>,
    pending: BTreeSet<String>,
}

/// What changed in memory since a [`Mark`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changes {
    /// New entries.
    pub saved: usize,
    /// Existing entries changed.
    pub updated: usize,
    /// New staged proposals awaiting `/memory approve`.
    pub proposed: usize,
}

impl Mark {
    /// The current entries and staged proposals.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn take(store: &Store) -> Result<Self, Error> {
        Ok(Self {
            entries: store
                .memory_entries(None)?
                .into_iter()
                .map(|e| (e.id, e.updated_at))
                .collect(),
            pending: store.memory_pending()?.into_iter().map(|p| p.id).collect(),
        })
    }

    /// What changed since the mark was taken.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn changes(&self, store: &Store) -> Result<Changes, Error> {
        let mut changes = Changes::default();
        for entry in store.memory_entries(None)? {
            match self.entries.get(&entry.id) {
                None => changes.saved += 1,
                Some(at) if *at != entry.updated_at => changes.updated += 1,
                Some(_) => {}
            }
        }
        changes.proposed = store
            .memory_pending()?
            .iter()
            .filter(|p| !self.pending.contains(&p.id))
            .count();
        Ok(changes)
    }
}

impl Changes {
    /// The summary line of a finished review.
    #[must_use]
    pub fn summary(&self) -> String {
        if *self == Self::default() {
            return "Memory: nothing changed.".to_owned();
        }
        let mut parts = Vec::new();
        for (count, what) in [(self.saved, "saved"), (self.updated, "updated")] {
            if count > 0 {
                parts.push(format!("{count} {what}"));
            }
        }
        if self.proposed > 0 {
            parts.push(format!("{} proposed (see /memory pending)", self.proposed));
        }
        format!("Memory: {}.", parts.join(", "))
    }
}

/// [`run`], measuring what the review changed in memory.
///
/// # Errors
///
/// Store or turn errors.
pub async fn refine(review: ReviewRun) -> Result<(Changes, TurnResult), Error> {
    let store = Arc::clone(&review.parent.setup().store);
    let mark = Mark::take(&store)?;
    let result = run(review).await?;
    Ok((mark.changes(&store)?, result))
}

/// Runs a background memory review of `review.session` in `review.child`.
///
/// # Errors
///
/// Store or turn errors.
pub async fn run(review: ReviewRun) -> Result<TurnResult, Error> {
    let parent = review.parent.setup();
    let scope = parent.scope.clone();
    let store = Arc::clone(&parent.store);
    let mut setup = MemorySetup::new(
        scope.clone(),
        Arc::clone(&store),
        Some(review.session),
        Source::Subagent,
    );
    setup.review = true;
    setup.redactor = parent.redactor.clone();
    let memory = Arc::new(MemorySession::new(setup));
    let tools = Arc::new(ReviewTools {
        memory: Arc::clone(&memory),
        definition: tool::definition(scope.config()),
    });
    let mut agent_setup = AgentSetup::new(
        scope.clone(),
        Arc::clone(&store),
        review.child,
        review.model,
        tools,
        PromptSetup::new("review", None),
    );
    agent_setup.unattended = true;
    agent_setup.interactive = false;
    let agent = Agent::new(agent_setup)?;
    let text = instruction(review.focus.as_deref(), &excerpt(&store, review.session)?);
    agent
        .run_turn(
            Some(&scope),
            review.child,
            TurnInput::text(text),
            TurnOptions::default(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(interval: u32) -> MemoryConfig {
        MemoryConfig {
            nudge_interval: interval,
            ..MemoryConfig::default()
        }
    }

    #[test]
    fn the_tenth_turn_without_a_memory_call_is_due() {
        let mut nudge = Nudge::new(&config(10), Source::Cli, 0);
        for _ in 0..9 {
            assert!(!nudge.observe(false, true));
        }
        assert!(nudge.observe(false, true));
        assert_eq!(nudge.count(), 0);
    }

    #[test]
    fn a_memory_call_resets_and_ineligible_sessions_never_review() {
        let mut nudge = Nudge::new(&config(2), Source::Cli, 0);
        assert!(!nudge.observe(false, true));
        assert!(!nudge.observe(true, true));
        assert!(!nudge.observe(false, true));
        assert!(nudge.observe(false, true));
        for source in [Source::Cron, Source::Subagent, Source::Harness] {
            let mut nudge = Nudge::new(&config(1), source, 0);
            assert!(!nudge.observe(false, true));
        }
        let mut off = Nudge::new(&config(0), Source::Cli, 0);
        assert!(!off.observe(false, true));
        let mut unavailable = Nudge::new(&config(1), Source::Cli, 0);
        assert!(!unavailable.observe(false, false));
    }

    #[test]
    fn a_resumed_session_restores_the_remainder() {
        let mut nudge = Nudge::new(&config(10), Source::Cli, 23);
        assert_eq!(nudge.count(), 3);
        for _ in 0..6 {
            assert!(!nudge.observe(false, true));
        }
        assert!(nudge.observe(false, true));
    }
}

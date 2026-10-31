//! Autonomous skill creation and improvement (spec: skills, Autonomous
//! skill creation and improvement).
//!
//! [`Nudge`] counts the tool-calling iterations of a session while
//! `skill_manage` is on offer; when a completed turn brings the count to
//! `skills.creation_nudge_interval`, the session runs a background review
//! after the reply is delivered ([`run`]). The review is its own hidden
//! child session with a restricted `skill_manage` ([`ReviewState`]): it may
//! only overwrite files it read with `skill_view` during the review, and
//! its deletes archive instead of removing.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use pmt_agent::turn::model::ModelPort;
use pmt_agent::turn::tools::ToolPort;
use pmt_agent::turn::{
    Agent, AgentSetup, ExitReason, PromptSetup, TurnInput, TurnOptions, TurnResult,
};
use pmt_config::schema::SkillsConfig;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Source;

pub use pmt_agent::review::{EXCERPT_CHARS, child_session, excerpt};

use crate::session::{SessionSetup, SkillSessions, SkillsSession};

/// Files read with `skill_view` during a review.
#[derive(Debug, Default)]
pub struct ReviewState {
    viewed: Mutex<BTreeSet<(String, String)>>,
}

impl ReviewState {
    /// Records that `file` of `skill` was read.
    pub fn mark_viewed(&self, skill: &str, file: &str) {
        self.viewed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((skill.to_owned(), file.to_owned()));
    }

    /// Whether `file` of `skill` was read.
    #[must_use]
    pub fn viewed(&self, skill: &str, file: &str) -> bool {
        self.viewed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&(skill.to_owned(), file.to_owned()))
    }
}

/// The refusal of an overwrite the review has not read.
#[must_use]
pub fn view_first(skill: &str, file: &str) -> String {
    if file == crate::layout::SKILL_FILE {
        format!("Read the skill first: call skill_view(name=\"{skill}\") before changing it.")
    } else {
        format!(
            "Read the file first: call skill_view(name=\"{skill}\", file_path=\"{file}\") before overwriting it."
        )
    }
}

/// Counts tool-calling iterations towards a review.
#[derive(Debug, Clone)]
pub struct Nudge {
    interval: u32,
    count: u32,
    eligible: bool,
}

impl Nudge {
    /// The counter of a session from `source` (cron, delegated children
    /// and harness evolution never review).
    #[must_use]
    pub fn new(config: &SkillsConfig, source: Source) -> Self {
        Self {
            interval: config.creation_nudge_interval,
            count: 0,
            eligible: !matches!(source, Source::Cron | Source::Subagent | Source::Harness),
        }
    }

    /// Adds a finished turn; `true` when a review is due now (the counter
    /// restarts).
    pub fn observe(&mut self, result: &TurnResult, skill_manage_available: bool) -> bool {
        let completed =
            result.exit_reason == ExitReason::Completed && result.final_response.is_some();
        self.observe_turn(result.iterations, completed, skill_manage_available)
    }

    /// [`Nudge::observe`] from the turn's iteration count and whether it
    /// completed with a final response.
    pub fn observe_turn(
        &mut self,
        iterations: u32,
        completed: bool,
        skill_manage_available: bool,
    ) -> bool {
        if !self.eligible || self.interval == 0 || !skill_manage_available {
            return false;
        }
        // The last iteration of a completed turn is the answer itself.
        let tool_iterations = iterations.saturating_sub(u32::from(completed));
        self.count = self.count.saturating_add(tool_iterations);
        if completed && self.count >= self.interval {
            self.count = 0;
            return true;
        }
        false
    }

    /// The current count.
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }
}

/// The review instruction.
pub const REVIEW_PROMPT: &str = "Review the conversation below for reusable know-how. If it \
shows a procedure worth repeating (a workflow, a fix, a convention) that no existing skill \
covers, create a skill with skill_manage; if an existing skill was wrong or incomplete, read it \
with skill_view and patch it. Keep descriptions short and concrete. If nothing is worth \
saving, answer \"nothing to save\" without calling tools.";

/// The reply to a review request while one runs.
pub const ALREADY_RUNNING: &str = "a review is already running";

/// What a background review runs with.
pub struct ReviewRun {
    /// The reviewed session's skills (scope, store, roots).
    pub parent: Arc<SkillsSession>,
    /// The reviewed session.
    pub session: SessionId,
    /// The review's hidden child session ([`child_session`]).
    pub child: SessionId,
    /// The model (normally the session's main route).
    pub model: Arc<dyn ModelPort>,
    /// The child's tools (normally the skills toolset only), bound to
    /// `child`.
    pub tools: Arc<dyn ToolPort>,
    /// The table the skill tools read sessions from.
    pub sessions: Arc<SkillSessions>,
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

/// Runs a background review in `review.child` with a review-mode
/// [`SkillsSession`] attached for the call. One review per session runs
/// at a time.
///
/// # Errors
///
/// `skill.review_running` while another review of the session runs;
/// store or turn errors.
pub async fn run(review: ReviewRun) -> Result<TurnResult, Error> {
    let _running = review
        .parent
        .begin_review()
        .ok_or_else(|| Error::new(&codes::SKILL_REVIEW_RUNNING, ALREADY_RUNNING))?;
    let parent = review.parent.setup();
    let store = Arc::clone(&parent.store);
    let child = review.child;
    let mut setup: SessionSetup = parent.clone();
    setup.session_id = child.to_string();
    setup.review = true;
    setup.secret_prompt = None;
    let _attached = review
        .sessions
        .attach(&child.to_string(), Arc::new(SkillsSession::new(setup)));
    let scope = parent.scope.clone();
    let mut agent_setup = AgentSetup::new(
        scope.clone(),
        Arc::clone(&store),
        child,
        review.model,
        review.tools,
        PromptSetup::new("review", None),
    );
    agent_setup.unattended = true;
    agent_setup.interactive = false;
    let agent = Agent::new(agent_setup)?;
    let text = instruction(review.focus.as_deref(), &excerpt(&store, review.session)?);
    agent
        .run_turn(
            Some(&scope),
            child,
            TurnInput::text(text),
            TurnOptions::default(),
        )
        .await
}

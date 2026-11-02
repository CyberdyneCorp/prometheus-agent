//! The background skill review a finished turn may trigger (spec: skills,
//! Autonomous skill creation and improvement); `/refine` itself is a
//! shared command ([`crate::chat::commands`]).

use std::sync::PoisonError;

use pmt_agent::turn::{ToolPort as _, TurnResult};

use super::Repl;
use crate::chat::session::Live;

impl Repl {
    /// Feeds a finished turn to the skill nudge and, when a review is due,
    /// runs it after the reply (spec: skills, Autonomous skill creation
    /// and improvement); a review already running skips this one.
    pub(super) async fn skill_nudge(&self, live: &Live, result: &TurnResult) {
        let available = live
            .tools
            .definitions()
            .iter()
            .any(|t| t.name == "skill_manage");
        let due = live
            .skill_nudge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(result, available);
        if !due {
            return;
        }
        let review = match crate::chat::commands::skill_review(&self.factory, live, None).await {
            Ok(review) => review,
            Err(err) => {
                tracing::warn!(error = %err, "skill review skipped");
                return;
            }
        };
        tokio::spawn(async move {
            if let Err(err) = pmt_skills::review::run(review).await {
                tracing::warn!(error = %err, "skill review failed");
            }
        });
    }
}

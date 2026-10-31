//! Payloads of the harness, harness-evolution and experience-library
//! events. Their producers arrive in M3; the grammar defines them now so
//! that every capability uses one set of names.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `harness.note`: a note the active harness adds to the trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessNote {
    /// The note.
    pub text: String,
}

/// `harness.activated`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessActivated {
    /// Previous harness version.
    pub from: String,
    /// Activated version.
    pub to: String,
    /// `rollback`, `adopt`, `manual`.
    pub source: String,
}

/// `evolve.job.started` and `evolve.job.finished`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvolveJob {
    /// The job id.
    pub job_id: String,
    /// `running`, `completed`, `failed`, `cancelled`.
    pub status: String,
}

/// `evolve.stage.finished`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvolveStage {
    /// The job id.
    pub job_id: String,
    /// The stage name.
    pub stage: String,
    /// The stage's outcome.
    pub status: String,
}

/// `evolve.rollout.finished`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EvolveRollout {
    /// The job id.
    pub job_id: String,
    /// The candidate harness.
    pub candidate: String,
    /// The task run.
    pub task: String,
    /// The rollout's score, when graded.
    pub score: Option<f64>,
}

/// `evolve.selection.finished`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvolveSelection {
    /// The job id.
    pub job_id: String,
    /// The selected candidate, if any.
    pub selected: Option<String>,
}

/// `evolve.proposal.ready` and `evolve.proposal.adopted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvolveProposal {
    /// The job id.
    pub job_id: String,
    /// The proposal id.
    pub proposal_id: String,
}

/// `experience.entry.added`, `experience.entry.evicted` and
/// `experience.entry.status_changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExperienceEntry {
    /// The entry id.
    pub entry_id: String,
    /// The entry's status after the event.
    pub status: String,
}

/// `experience.lesson.distilled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LessonDistilled {
    /// The distillation job.
    pub job: String,
    /// The distillation mode.
    pub mode: String,
    /// Confidence 0 to 1, `null` for none.
    pub confidence: Option<f64>,
}

/// `experience.session_lessons.injected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LessonsInjected {
    /// The session the lessons were injected into.
    pub session: String,
    /// The injected entry ids.
    pub entry_ids: Vec<String>,
}

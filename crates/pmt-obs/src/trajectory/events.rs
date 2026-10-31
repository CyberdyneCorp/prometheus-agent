//! The closed set of trajectory event types of grammar 1 and their typed
//! payloads (spec: observability, Trajectory event types). One table
//! declares each type's name, variant and payload; the enum, the name list
//! and the schema are generated from it, so they cannot disagree.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::Value;

use super::payload::{
    ApprovalDecision, ApprovalRequest, AuxCallEnd, AuxCallStart, BatchPlanned, Budget, CacheBreak,
    ClarifyAnswer, ClarifyRequest, CompressionCommitted, CompressionFailed, EgressDenied,
    ErrorEvent, EvolveJob, EvolveProposal, EvolveRollout, EvolveSelection, EvolveStage,
    ExperienceEntry, HarnessActivated, HarnessNote, LessonDistilled, LessonsInjected,
    MemoryRecalled, MessageText, ModelCall, PromptRebuilt, ProviderFallback, Reasoning,
    RegistryReloaded, RouteSelected, SecurityDowngrade, SessionEnd, SessionStart, SubagentFinished,
    SubagentProgress, SubagentSpawned, ToolCall, ToolResult, ToolsRebuilt, TurnEnd, TurnStart,
    TurnStep,
};

macro_rules! events {
    ($($variant:ident = $name:literal => $payload:ty;)*) => {
        /// A trajectory event of a type this build knows, with its payload.
        #[derive(Debug, Clone, PartialEq)]
        #[allow(missing_docs)] // each variant is named after its event type
        pub enum Event {
            $($variant($payload),)*
        }

        /// Every event type name of the grammar, in spec order.
        pub const EVENT_TYPES: &[&str] = &[$($name),*];

        impl Event {
            /// The event type name (`tool.result`).
            #[must_use]
            pub fn kind(&self) -> &'static str {
                match self {
                    $(Self::$variant(_) => $name,)*
                }
            }

            /// The payload as JSON.
            #[must_use]
            pub fn payload(&self) -> Value {
                // Payloads hold only strings, numbers, booleans, lists and
                // JSON values, which always serialize.
                match self {
                    $(Self::$variant(p) => serde_json::to_value(p).unwrap_or(Value::Null),)*
                }
            }

            /// Decodes a payload of type `kind`; `None` for a type this
            /// build does not know.
            pub(crate) fn decode(kind: &str, payload: Value) -> Option<serde_json::Result<Self>> {
                match kind {
                    $($name => Some(serde_json::from_value(payload).map(Self::$variant)),)*
                    _ => None,
                }
            }
        }

        /// `(type name, payload schema)` for every event type.
        pub(crate) fn payload_schemas(generator: &mut SchemaGenerator) -> Vec<(&'static str, Schema)> {
            vec![$(($name, <$payload as JsonSchema>::json_schema(generator)),)*]
        }
    };
}

events! {
    SessionStart = "session.start" => SessionStart;
    SessionEnd = "session.end" => SessionEnd;
    SessionToolsRebuilt = "session.tools_rebuilt" => ToolsRebuilt;
    TurnStart = "turn.start" => TurnStart;
    TurnStep = "turn.step" => TurnStep;
    TurnEnd = "turn.end" => TurnEnd;
    UserMessage = "user.message" => MessageText;
    AssistantMessage = "assistant.message" => MessageText;
    AssistantReasoning = "assistant.reasoning" => Reasoning;
    ModelCall = "model.call" => ModelCall;
    AuxCallStart = "aux_call.start" => AuxCallStart;
    AuxCallEnd = "aux_call.end" => AuxCallEnd;
    ToolCall = "tool.call" => ToolCall;
    ToolResult = "tool.result" => ToolResult;
    ToolBatchPlanned = "tool.batch_planned" => BatchPlanned;
    ApprovalRequest = "approval.request" => ApprovalRequest;
    ApprovalDecision = "approval.decision" => ApprovalDecision;
    ClarifyRequest = "clarify.request" => ClarifyRequest;
    ClarifyAnswer = "clarify.answer" => ClarifyAnswer;
    SubagentSpawned = "subagent.spawned" => SubagentSpawned;
    SubagentProgress = "subagent.progress" => SubagentProgress;
    SubagentFinished = "subagent.finished" => SubagentFinished;
    CompressionCommitted = "compression.committed" => CompressionCommitted;
    CompressionFailed = "compression.failed" => CompressionFailed;
    PromptRebuilt = "prompt.rebuilt" => PromptRebuilt;
    CacheUnexpectedBreak = "cache.unexpected_break" => CacheBreak;
    ProviderFallback = "provider.fallback" => ProviderFallback;
    ProviderRouteSelected = "provider.route_selected" => RouteSelected;
    ProviderRegistryReloaded = "provider.registry_reloaded" => RegistryReloaded;
    MemoryRecalled = "memory.recalled" => MemoryRecalled;
    BudgetWarning = "budget.warning" => Budget;
    BudgetExceeded = "budget.exceeded" => Budget;
    EgressDenied = "egress.denied" => EgressDenied;
    SecurityDowngrade = "security.downgrade" => SecurityDowngrade;
    HarnessNote = "harness.note" => HarnessNote;
    HarnessActivated = "harness.activated" => HarnessActivated;
    EvolveJobStarted = "evolve.job.started" => EvolveJob;
    EvolveJobFinished = "evolve.job.finished" => EvolveJob;
    EvolveStageFinished = "evolve.stage.finished" => EvolveStage;
    EvolveRolloutFinished = "evolve.rollout.finished" => EvolveRollout;
    EvolveSelectionFinished = "evolve.selection.finished" => EvolveSelection;
    EvolveProposalReady = "evolve.proposal.ready" => EvolveProposal;
    EvolveProposalAdopted = "evolve.proposal.adopted" => EvolveProposal;
    ExperienceEntryAdded = "experience.entry.added" => ExperienceEntry;
    ExperienceEntryEvicted = "experience.entry.evicted" => ExperienceEntry;
    ExperienceEntryStatusChanged = "experience.entry.status_changed" => ExperienceEntry;
    ExperienceLessonDistilled = "experience.lesson.distilled" => LessonDistilled;
    ExperienceSessionLessonsInjected = "experience.session_lessons.injected" => LessonsInjected;
    Error = "error" => ErrorEvent;
}

#[cfg(test)]
mod tests {
    use super::EVENT_TYPES;

    /// The list in spec: observability, Trajectory event types.
    const SPEC: &str = "session.start session.end session.tools_rebuilt turn.start turn.step \
        turn.end user.message assistant.message assistant.reasoning model.call aux_call.start \
        aux_call.end tool.call tool.result tool.batch_planned approval.request approval.decision \
        clarify.request clarify.answer subagent.spawned subagent.progress subagent.finished \
        compression.committed compression.failed prompt.rebuilt cache.unexpected_break \
        provider.fallback provider.route_selected provider.registry_reloaded memory.recalled \
        budget.warning budget.exceeded egress.denied security.downgrade harness.note \
        harness.activated evolve.job.started evolve.job.finished evolve.stage.finished \
        evolve.rollout.finished evolve.selection.finished evolve.proposal.ready \
        evolve.proposal.adopted experience.entry.added experience.entry.evicted \
        experience.entry.status_changed experience.lesson.distilled \
        experience.session_lessons.injected error";

    #[test]
    fn the_grammar_defines_exactly_the_spec_event_types() {
        let spec: Vec<&str> = SPEC.split_whitespace().collect();
        assert_eq!(EVENT_TYPES, spec.as_slice());
    }
}

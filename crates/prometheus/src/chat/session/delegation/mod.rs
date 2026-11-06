//! Subagent delegation in a running session (spec: subagent-delegation):
//! every session gets a [`pmt_delegation::host::Delegator`] frozen at its
//! start, whose children are real sessions opened here
//! ([`launcher::ChildLauncher`]) with a narrowed grant
//! ([`ChildSpec`]): exactly the granted tools and capabilities, no
//! context files, no clarify channel, the parent's frozen memory snapshot,
//! an ephemeral system prompt and approvals that fail closed.

mod endpoint;
pub mod isolation;
mod launcher;
pub mod routers;
mod wiring;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::{EventSink, PromptSetup};
use pmt_config::ProfileScope;
use pmt_config::schema::ChildApproval;
use pmt_core::capability::CapabilitySet;
use pmt_core::error::Error;
use pmt_core::human::{Channel, Surface};
use pmt_core::id::SessionId;
use pmt_delegation::budget::OneShotBudget;
use pmt_tools::Selection;
use pmt_tools::approval::{ApprovalRequest, CommandGate, Decision};
use pmt_tools::environments::tool_output_reads;

pub use launcher::ChildLauncher;
pub use wiring::{PAUSE_FILE, Wiring};

use super::model::RouteFlags;
use super::{Factory, tools};

/// The in-process line chat's delegation inbox: a completion becomes the
/// chat's next input once it is idle.
pub struct LineInbox(pub tokio::sync::mpsc::UnboundedSender<String>);

impl pmt_delegation::host::Completions for LineInbox {
    fn deliver<'a>(
        &'a self,
        _: &'a str,
        text: String,
    ) -> pmt_agent::turn::model::BoxFuture<'a, bool> {
        let sent = self.0.send(text).is_ok();
        Box::pin(async move { sent })
    }
}

/// The route flags of a factory, owned.
#[derive(Debug, Clone, Default)]
pub struct OwnedFlags {
    provider: Option<String>,
    model: Option<String>,
    mock_script: Option<PathBuf>,
}

impl OwnedFlags {
    /// `flags`, owned.
    #[must_use]
    pub fn of(flags: &RouteFlags<'_>) -> Self {
        Self {
            provider: flags.provider.map(str::to_owned),
            model: flags.model.map(str::to_owned),
            mock_script: flags.mock_script.map(Path::to_path_buf),
        }
    }

    /// The flags, borrowed.
    #[must_use]
    pub fn borrow(&self) -> RouteFlags<'_> {
        RouteFlags {
            provider: self.provider.as_deref(),
            model: self.model.as_deref(),
            mock_script: self.mock_script.as_deref(),
        }
    }
}

/// What makes a session a delegated child.
pub struct ChildSpec {
    /// `sa-<ULID>`.
    pub subagent_id: String,
    /// Depth from 1.
    pub depth: u32,
    /// It runs on a model of its own (no inherited fallback chain).
    pub pinned: bool,
    /// Exactly its tools.
    pub tools: Vec<String>,
    /// Its capabilities (already narrowed).
    pub capabilities: CapabilitySet,
    /// Its ephemeral system prompt.
    pub system: String,
    /// The parent's frozen memory snapshot.
    pub memory: Option<String>,
    /// Model calls it may make.
    pub max_iterations: u32,
    /// Its profile's configuration with its budget share as session budget.
    pub scope: ProfileScope,
    /// How its approvals resolve.
    pub approval: ChildApproval,
    /// How long an escalated approval waits.
    pub escalation_timeout: Duration,
    /// The parent's gate (escalations go up through it).
    pub parent_gate: Option<Arc<CommandGate>>,
    /// The root session's event stream (its own children report there).
    pub root_events: Arc<dyn EventSink>,
    /// The run's one-shot spawn budget, shared with the parent.
    pub oneshot: Option<Arc<OneShotBudget>>,
    /// `delegation.request_overrides`, set on every model call.
    pub body_overrides: Option<Arc<serde_json::Map<String, serde_json::Value>>>,
}

impl ChildSpec {
    /// Exactly the granted tools.
    #[must_use]
    pub fn selection(&self) -> Selection {
        Selection {
            toolsets: Vec::new(),
            tools: self.tools.clone(),
            disabled: std::collections::BTreeSet::new(),
            posture: false,
            session_opt_ins: std::collections::BTreeSet::new(),
        }
    }

    /// Its capabilities, plus reading its own tool output.
    #[must_use]
    pub fn capabilities(&self, profile_dir: &Path, id: SessionId) -> CapabilitySet {
        self.capabilities
            .union(&tool_output_reads(profile_dir, &id.to_string()))
    }

    /// No context files and no pinned skills; the ephemeral prompt and the
    /// parent's frozen memory instead of its own.
    pub fn shape_prompt(&self, prompt: &mut PromptSetup) {
        prompt.no_context_files = true;
        prompt.pinned_skills = None;
        prompt.caller_system = Some(self.system.clone());
        prompt.memory.clone_from(&self.memory);
    }

    /// Its configuration snapshot.
    #[must_use]
    pub fn scope(&self) -> ProfileScope {
        self.scope.clone()
    }
}

impl Factory {
    /// Opens child session `id` on `target` with `spec`'s narrowing,
    /// sending its events to `clients`.
    ///
    /// # Errors
    ///
    /// As [`Factory::open`].
    pub(crate) async fn open_child(
        &self,
        id: SessionId,
        target: &pmt_providers::resolve::Target,
        spec: &ChildSpec,
        clients: super::Clients,
    ) -> Result<super::Live, Error> {
        self.open_inner(id, target, Some(clients), Some(spec)).await
    }

    /// A child's approval gate: every approval goes to a forwarder that
    /// denies it (`delegation.child_approval = "deny"`) or escalates it to
    /// the root session's surface tagged with the child's id, denying it
    /// when nobody answers in time.
    pub(super) fn child_gate(
        &self,
        id: SessionId,
        spec: &ChildSpec,
        environment: &super::env::Attached,
    ) -> Result<tools::Gate, Error> {
        let wait = spec.escalation_timeout + Duration::from_secs(5);
        let (channel, surface) = Channel::attached(wait);
        let gate = tools::child_gate(
            &tools::GateInputs {
                scope: &self.scope,
                grants: &self.grants,
                cwd: &self.options.cwd,
                user_home: self.launch.user_home().map(PathBuf::as_path),
                interactive: true,
                cron: false,
                yolo: false,
                hooks: None,
                session_id: id.to_string(),
                guardian: None,
                previewer: environment.previewer.clone(),
                hosted: self
                    .daemon
                    .clone()
                    .map(|daemon| (daemon, super::own_processes(environment.terminal.as_ref()))),
            },
            channel,
        )?;
        tokio::spawn(forward(
            surface,
            spec.subagent_id.clone(),
            spec.approval,
            spec.parent_gate.clone().map(|g| g as Arc<dyn Escalate>),
            spec.escalation_timeout,
        ));
        Ok(tools::Gate {
            gate,
            prompts: None,
        })
    }
}

/// Where a child's approval is escalated: its parent's gate (and through
/// it the root session's surface).
pub trait Escalate: Send + Sync {
    /// Asks; `None` when nobody answered.
    fn ask(
        &self,
        request: ApprovalRequest,
    ) -> pmt_agent::turn::model::BoxFuture<'_, Option<Decision>>;
}

impl Escalate for CommandGate {
    fn ask(
        &self,
        request: ApprovalRequest,
    ) -> pmt_agent::turn::model::BoxFuture<'_, Option<Decision>> {
        Box::pin(async move { self.escalate(request).await.ok() })
    }
}

/// Answers a child's approval prompts: denied, or escalated to the parent
/// gate's surface and denied when unanswered within `timeout`. Every
/// denial is logged with the child's id.
async fn forward(
    mut prompts: Surface<ApprovalRequest, Decision>,
    subagent_id: String,
    policy: ChildApproval,
    parent: Option<Arc<dyn Escalate>>,
    timeout: Duration,
) {
    while let Some(prompt) = prompts.next().await {
        let request = prompt.request().clone();
        let decision = match (policy, &parent) {
            (ChildApproval::Escalate, Some(gate)) => {
                let tagged = tagged(request.clone(), &subagent_id);
                match tokio::time::timeout(timeout, gate.ask(tagged)).await {
                    Ok(Some(decision)) => decision,
                    _ => Decision::Deny,
                }
            }
            _ => Decision::Deny,
        };
        if decision == Decision::Deny {
            tracing::warn!(
                subagent_id = %subagent_id,
                tool = %request.tool,
                subject = request.subject.as_deref().unwrap_or_default(),
                policy = ?policy,
                "subagent approval denied"
            );
        }
        prompt.respond(decision);
    }
}

/// `request` labelled with the child it comes from.
fn tagged(mut request: ApprovalRequest, subagent_id: &str) -> ApprovalRequest {
    let subject = request.subject.take().unwrap_or_default();
    request.subject = Some(format!("[subagent {subagent_id}] {subject}"));
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Silent;

    impl Escalate for Silent {
        fn ask(
            &self,
            _: ApprovalRequest,
        ) -> pmt_agent::turn::model::BoxFuture<'_, Option<Decision>> {
            Box::pin(std::future::pending())
        }
    }

    struct Approves;

    impl Escalate for Approves {
        fn ask(
            &self,
            request: ApprovalRequest,
        ) -> pmt_agent::turn::model::BoxFuture<'_, Option<Decision>> {
            assert!(request.subject.unwrap().starts_with("[subagent sa-1] "));
            Box::pin(async { Some(Decision::Once) })
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            tool: "terminal".into(),
            call_id: "c1".into(),
            subject: Some("rm -rf build".into()),
            findings: Vec::new(),
            remember: Some("rm -rf".into()),
            explanation: None,
        }
    }

    async fn decide(policy: ChildApproval, parent: Option<Arc<dyn Escalate>>) -> Decision {
        let (channel, surface) = Channel::attached(Duration::from_hours(1));
        tokio::spawn(forward(
            surface,
            "sa-1".into(),
            policy,
            parent,
            Duration::from_mins(5),
        ));
        channel.ask(request()).await.unwrap()
    }

    /// Scenario: Escalation times out to deny.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_escalation_is_denied_after_its_timeout() {
        let started = tokio::time::Instant::now();
        assert_eq!(
            decide(ChildApproval::Escalate, Some(Arc::new(Silent))).await,
            Decision::Deny
        );
        assert!(started.elapsed() >= Duration::from_mins(5));
    }

    #[tokio::test]
    async fn escalations_reach_the_parent_tagged_and_deny_needs_nobody() {
        assert_eq!(
            decide(ChildApproval::Escalate, Some(Arc::new(Approves))).await,
            Decision::Once
        );
        assert_eq!(
            decide(ChildApproval::Deny, Some(Arc::new(Approves))).await,
            Decision::Deny
        );
    }
}

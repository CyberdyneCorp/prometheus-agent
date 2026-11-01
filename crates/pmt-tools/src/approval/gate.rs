//! The session's approval gate: the dispatcher asks it about every call.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::grants::SessionGrants;
use pmt_config::schema::{ApprovalMode, ApprovalsConfig, UnattendedApproval};
use pmt_core::capability::Capability;
use pmt_core::human::{Channel, Unanswered};
use serde::Serialize;
use serde_json::Value;

use super::rules::{PLUGIN_RULE_PREFIX, literal};
use super::{Context, Finding, Floors, Policy, Verdict, evaluate};
use crate::descriptor::{PathClaim, RegisteredTool};
use crate::dispatch::{ApprovalGate, Escalation, GateRequest, GateVerdict, SeamFuture};
use crate::files::guards::{absolute, device_namespace, resolve_existing};
use crate::files::patch::v4a::touched_paths;
use crate::grants::GrantAnswer;
use crate::protected::ProtectedFiles;

/// A human's decision: `once`, `session`, `always` or `deny` (the
/// `decision` of `approval.respond`; the same choices as grant prompts).
pub type Decision = GrantAnswer;

/// What the model is told after any denial.
pub const STOP: &str = "STOP: do NOT re-run, rephrase, or re-issue this command";

/// The detector name of protected instruction file writes.
const PROTECTED: &str = "protected-file";

/// One approval prompt (the payload of the `approval.request` event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalRequest {
    /// The tool called.
    pub tool: String,
    /// The model's call id.
    pub call_id: String,
    /// The command line or written path, when there is one.
    pub subject: Option<String>,
    /// Why approval is needed.
    pub findings: Vec<Finding>,
    /// What `session` and `always` record: a command pattern or
    /// `plugin_rule:<key>`. `None` for protected instruction files, whose
    /// writes ask every time.
    pub remember: Option<String>,
}

impl ApprovalRequest {
    /// The findings in one line.
    #[must_use]
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self.findings.iter().map(ToString::to_string).collect();
        parts.join("; ")
    }
}

/// What a session's gate is built from.
#[derive(Debug)]
pub struct GateSetup {
    /// `[approvals]` of the session's profile.
    pub approvals: ApprovalsConfig,
    /// Who can answer.
    pub context: Context,
    /// Paths hardline rules protect and the working directory.
    pub floors: Floors,
    /// The session's grants (`[approvals].allow` is read through them, so a
    /// hand edit of `grants.toml` reaches the next check).
    pub grants: Arc<SessionGrants>,
    /// Protected instruction files.
    pub protected: ProtectedFiles,
    /// The human channel (`Channel::unattended()` when none exists).
    pub channel: Channel<ApprovalRequest, Decision>,
    /// `--yolo` for the whole process.
    pub yolo: bool,
}

/// The approval gate of one session.
#[derive(Debug)]
pub struct CommandGate {
    setup: GateSetup,
    yolo: AtomicBool,
    session: Mutex<Vec<String>>,
    unobtainable: AtomicU32,
}

impl CommandGate {
    /// A gate; nothing is approved for the session yet.
    #[must_use]
    pub fn new(setup: GateSetup) -> Self {
        let yolo = AtomicBool::new(setup.yolo);
        Self {
            setup,
            yolo,
            session: Mutex::new(Vec::new()),
            unobtainable: AtomicU32::new(0),
        }
    }

    /// How many calls this gate denied because they needed an approval
    /// nobody could give (a non-interactive run whose mode denies). A
    /// one-shot run ends with exit 4 once this is non-zero (spec: cli,
    /// One-shot and piped mode).
    #[must_use]
    pub fn unobtainable(&self) -> u32 {
        self.unobtainable.load(Ordering::Relaxed)
    }

    /// `/yolo`: bypass on or off for the rest of the session.
    pub fn set_yolo(&self, on: bool) {
        self.yolo.store(on, Ordering::Relaxed);
    }

    /// Whether yolo is active (the status line shows `YOLO` while it is).
    #[must_use]
    pub fn yolo(&self) -> bool {
        self.yolo.load(Ordering::Relaxed)
    }

    /// The status-line badge: `Some("YOLO")` while yolo is active.
    #[must_use]
    pub fn status_badge(&self) -> Option<&'static str> {
        self.yolo().then_some("YOLO")
    }

    /// Whether approvals are bypassed (`mode = "off"` or yolo).
    #[must_use]
    pub fn bypassing(&self) -> bool {
        self.yolo() || self.setup.approvals.mode == ApprovalMode::Off
    }

    /// `[approvals].allow` as on disk now, then this session's approvals.
    /// An unreadable `grants.toml` allows nothing (more prompts, never
    /// fewer).
    fn allow_rules(&self) -> Vec<String> {
        let mut rules = self.setup.grants.allowed_commands().unwrap_or_else(|err| {
            tracing::error!(error = %err, "grants.toml unreadable; no command is allowlisted");
            Vec::new()
        });
        rules.extend(self.lock().iter().cloned());
        rules
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gates one command line of `tool`.
    pub async fn check_command(&self, tool: &str, call_id: &str, line: &str) -> GateVerdict {
        let allow = self.allow_rules();
        let policy = Policy {
            bypass: self.bypassing(),
            deny: &self.setup.approvals.deny,
            allow: &allow,
            floors: &self.setup.floors,
        };
        let verdict = match evaluate(line, &policy) {
            Ok(verdict) => verdict,
            Err(err) => {
                return GateVerdict::Blocked(format!(
                    "BLOCKED: the command could not be read ({err}); fix its quoting"
                ));
            }
        };
        if let Some(message) = verdict.blocked_message() {
            return GateVerdict::Blocked(message);
        }
        let Verdict::Prompt(findings) = verdict else {
            return GateVerdict::Allow;
        };
        self.decide(ApprovalRequest {
            tool: tool.to_owned(),
            call_id: call_id.to_owned(),
            subject: Some(line.to_owned()),
            findings,
            remember: Some(literal(line.trim())),
        })
        .await
    }

    async fn check_escalation(
        &self,
        request: &GateRequest<'_>,
        escalation: &Escalation,
    ) -> GateVerdict {
        let key = format!("{PLUGIN_RULE_PREFIX}{}", escalation.rule_key);
        if self.bypassing() || self.allow_rules().contains(&key) {
            return GateVerdict::Allow;
        }
        self.decide(ApprovalRequest {
            tool: request.tool.name().to_owned(),
            call_id: request.call_id.to_owned(),
            subject: None,
            findings: vec![Finding {
                detector: key.clone(),
                reason: escalation.reason.clone(),
                fragment: request.tool.name().to_owned(),
            }],
            remember: Some(key),
        })
        .await
    }

    /// Writes to protected instruction files ask in every mode. A written
    /// device-namespace or UNC path is refused here, before the protected
    /// check resolves it (which would open it on Windows), in every mode.
    async fn check_protected(&self, request: &GateRequest<'_>) -> GateVerdict {
        let floors = &self.setup.floors;
        let user_home = floors.user_home.as_deref().map(Path::new);
        for path in written_paths(request.tool, request.arguments) {
            let absolute = absolute(&path, Path::new(&floors.cwd), user_home);
            if let Some(refusal) =
                device_namespace(&path).or_else(|| device_namespace(&absolute.to_string_lossy()))
            {
                return GateVerdict::Denied(refusal);
            }
            let reason = self
                .setup
                .protected
                .requires_approval(&absolute)
                .map(|protected| protected.reason())
                .or_else(|| ssh_config(&absolute, user_home));
            let Some(reason) = reason else {
                continue;
            };
            let verdict = self
                .decide(ApprovalRequest {
                    tool: request.tool.name().to_owned(),
                    call_id: request.call_id.to_owned(),
                    subject: Some(path.clone()),
                    findings: vec![Finding {
                        detector: PROTECTED.to_owned(),
                        reason,
                        fragment: path,
                    }],
                    remember: None,
                })
                .await;
            if verdict != GateVerdict::Allow {
                return verdict;
            }
        }
        GateVerdict::Allow
    }

    /// The decision step: the human in an interactive session, the typed
    /// mode elsewhere. Never waits outside interactive sessions.
    async fn decide(&self, request: ApprovalRequest) -> GateVerdict {
        let summary = request.summary();
        let context = self.setup.context;
        let mode = match context {
            Context::Interactive => return self.ask(request).await,
            Context::Cron => self.setup.approvals.cron_mode,
            Context::SingleQuery => self.setup.approvals.single_query_mode,
            Context::Unattended => self.setup.approvals.unattended_mode,
        };
        if request.remember.is_none() || mode == UnattendedApproval::Deny {
            self.unobtainable.fetch_add(1, Ordering::Relaxed);
        }
        match (request.remember.is_some(), mode) {
            (false, _) => GateVerdict::Denied(format!(
                "BLOCKED: {summary}. Protected instruction files need a human approval, which {context} runs cannot give."
            )),
            (true, UnattendedApproval::Approve) => GateVerdict::Allow,
            (true, UnattendedApproval::Deny) => GateVerdict::Denied(format!(
                "BLOCKED: {summary}. To allow this in {context} runs set approvals.{context}_mode = \"approve\" in config.toml."
            )),
        }
    }

    async fn ask(&self, request: ApprovalRequest) -> GateVerdict {
        let summary = request.summary();
        let remember = request.remember.clone();
        let denied = |why: &str| GateVerdict::Denied(format!("DENIED: {summary} ({why}). {STOP}"));
        match self.setup.channel.ask(request).await {
            Ok(Decision::Once) => GateVerdict::Allow,
            Ok(Decision::Session) => {
                self.lock().extend(remember);
                GateVerdict::Allow
            }
            Ok(Decision::Always) => {
                if let Some(pattern) = remember {
                    self.remember_always(&pattern);
                }
                GateVerdict::Allow
            }
            Ok(Decision::Deny) => denied("denied by the user"),
            Err(Unanswered::TimedOut(after)) => denied(&format!(
                "no answer within {} s (approvals.timeout_secs)",
                after.as_secs()
            )),
            Err(other) => denied(&other.to_string()),
        }
    }

    /// Records an `always` decision in `grants.toml` (merged with the file
    /// as it is now) and for this session. The call was approved either
    /// way; a failed write is logged and only costs a later prompt.
    fn remember_always(&self, pattern: &str) {
        let file = self.setup.grants.persistent().file();
        if let Err(err) = file.allow_command(pattern) {
            tracing::error!(error = %err, pattern, "could not record the approval in grants.toml");
        }
        self.lock().push(pattern.to_owned());
    }
}

impl ApprovalGate for CommandGate {
    fn check<'a>(&'a self, request: GateRequest<'a>) -> SeamFuture<'a, GateVerdict> {
        Box::pin(async move {
            if let Some(line) = command_line(request.tool, request.arguments) {
                let verdict = self
                    .check_command(request.tool.name(), request.call_id, line)
                    .await;
                if verdict != GateVerdict::Allow {
                    return verdict;
                }
            }
            if let Some(escalation) = request.escalation {
                let verdict = self.check_escalation(&request, escalation).await;
                if verdict != GateVerdict::Allow {
                    return verdict;
                }
            }
            self.check_protected(&request).await
        })
    }
}

/// The command line of a tool that spawns processes (terminal, code
/// execution): its `command` (or `code`) argument.
fn command_line<'a>(tool: &RegisteredTool, arguments: &'a Value) -> Option<&'a str> {
    let spawns = tool
        .descriptor
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::ProcessSpawn(_)));
    if !spawns {
        return None;
    }
    ["command", "code"]
        .iter()
        .find_map(|key| arguments.get(key).and_then(Value::as_str))
}

/// `~/.ssh/config` steers every SSH connection (proxy commands run
/// programs), so writes to it always ask, like protected instruction files.
fn ssh_config(path: &Path, user_home: Option<&Path>) -> Option<String> {
    let config = user_home?.join(".ssh").join("config");
    let hit = path == config || resolve_existing(path) == resolve_existing(&config);
    hit.then(|| {
        format!(
            "{} is the SSH client configuration; writes always need human approval",
            path.display()
        )
    })
}

/// The paths a call writes, from its argument write claims.
fn written_paths(tool: &RegisteredTool, arguments: &Value) -> Vec<String> {
    let text = |pointer: &str| arguments.pointer(pointer).and_then(Value::as_str);
    tool.descriptor
        .claims
        .iter()
        .flat_map(|claims| &claims.writes)
        .flat_map(|claim| match claim {
            PathClaim::Argument(pointer) | PathClaim::ArgumentOrCwd(pointer) => {
                text(pointer).map(str::to_owned).into_iter().collect()
            }
            PathClaim::PatchText(pointer) => text(pointer).map(touched_paths).unwrap_or_default(),
            PathClaim::Fixed(_) => Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests;

//! The session's approval gate: the dispatcher asks it about every call.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::grants::SessionGrants;
use pmt_config::schema::{ApprovalMode, ApprovalsConfig, UnattendedApproval};
use pmt_core::audit::{AuditLog, Event, Kind};
use pmt_core::capability::Capability;
use pmt_core::human::{Channel, Unanswered};
use serde::Serialize;
use serde_json::Value;

use super::explain::Previewer;
use super::guardian::{Breaker, Guardian, GuardianVerdict};
use super::hooks::{ApprovalHooks, TransportOutcome, TransportRequest, choice_name};
use super::rules::{PLUGIN_RULE_PREFIX, literal};
use super::selfterm::{Daemon, Guarded};
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

/// How many times one prompt may be explained before it counts as a
/// denial (a client answering `explain` forever must not hold the call).
const EXPLAINS: usize = 5;

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
    /// What `explain` showed, when the request is asked again after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
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
    /// Plugin observers and the selected approval transport.
    pub hooks: Option<Arc<dyn ApprovalHooks>>,
}

/// The pids of the background processes a session started, asked at
/// every check (they change while the session runs).
pub type OwnProcesses = Arc<dyn Fn() -> Vec<u32> + Send + Sync>;

/// The daemon a session is hosted by and the session's own processes.
#[derive(Clone)]
struct Hosted {
    daemon: Daemon,
    own: OwnProcesses,
}

impl std::fmt::Debug for Hosted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hosted")
            .field("daemon", &self.daemon)
            .finish_non_exhaustive()
    }
}

/// The approval gate of one session.
#[derive(Debug)]
pub struct CommandGate {
    setup: GateSetup,
    /// `[approvals]` as of now: a live key, replaced when the profile's
    /// configuration is reloaded.
    approvals: std::sync::RwLock<ApprovalsConfig>,
    yolo: AtomicBool,
    session: Mutex<Vec<String>>,
    unobtainable: AtomicU32,
    /// Where decisions are recorded, with the session id.
    audit: Option<(AuditLog, String)>,
    /// The smart-mode guardian, when the session has one.
    guardian: Option<Arc<dyn Guardian>>,
    /// Runs `explain` previews (read-only, offline).
    previewer: Option<Previewer>,
    /// Consecutive guardian denials.
    breaker: Breaker,
    /// The hosting daemon, in a daemon-hosted session.
    hosted: Option<Hosted>,
}

impl CommandGate {
    /// A gate; nothing is approved for the session yet.
    #[must_use]
    pub fn new(setup: GateSetup) -> Self {
        let yolo = AtomicBool::new(setup.yolo);
        Self {
            approvals: std::sync::RwLock::new(setup.approvals.clone()),
            setup,
            yolo,
            session: Mutex::new(Vec::new()),
            unobtainable: AtomicU32::new(0),
            audit: None,
            guardian: None,
            breaker: Breaker::default(),
            previewer: None,
            hosted: None,
        }
    }

    /// Blocks commands that would stop `daemon`, the process hosting this
    /// session, under every mode; kills of `own` processes are left to the
    /// rest of the gate.
    #[must_use]
    pub fn hosted_by(mut self, daemon: Daemon, own: OwnProcesses) -> Self {
        self.hosted = Some(Hosted { daemon, own });
        self
    }

    /// Runs the previews of `explain` with `previewer`.
    #[must_use]
    pub fn with_previewer(mut self, previewer: Previewer) -> Self {
        self.previewer = Some(previewer);
        self
    }

    /// Asks `guardian` first in `smart` mode.
    #[must_use]
    pub fn with_guardian(mut self, guardian: Arc<dyn Guardian>) -> Self {
        self.guardian = Some(guardian);
        self
    }

    /// Records every decision of this gate in the profile's security audit
    /// log under `session`.
    #[must_use]
    pub fn with_audit(mut self, log: AuditLog, session: impl Into<String>) -> Self {
        self.audit = Some((log, session.into()));
        self
    }

    /// How many calls this gate denied because they needed an approval
    /// nobody could give (a non-interactive run whose mode denies). A
    /// one-shot run ends with exit 4 once this is non-zero (spec: cli,
    /// One-shot and piped mode).
    #[must_use]
    pub fn unobtainable(&self) -> u32 {
        self.unobtainable.load(Ordering::Relaxed)
    }

    /// Applies a reloaded `[approvals]` (an apply class `live` section) to
    /// the running session.
    pub fn set_approvals(&self, approvals: ApprovalsConfig) {
        *self
            .approvals
            .write()
            .unwrap_or_else(PoisonError::into_inner) = approvals;
    }

    fn approvals(&self) -> std::sync::RwLockReadGuard<'_, ApprovalsConfig> {
        self.approvals
            .read()
            .unwrap_or_else(PoisonError::into_inner)
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
        self.yolo() || self.approvals().mode == ApprovalMode::Off
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
        let deny = self.approvals().deny.clone();
        let own = self.hosted.as_ref().map(|h| (h.own)()).unwrap_or_default();
        let policy = Policy {
            bypass: self.bypassing(),
            deny: &deny,
            allow: &allow,
            floors: &self.setup.floors,
            daemon: self.hosted.as_ref().map(|h| Guarded {
                daemon: &h.daemon,
                own: &own,
            }),
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
            explanation: None,
        })
        .await
    }

    async fn check_escalation(
        &self,
        request: &GateRequest<'_>,
        escalation: &Escalation,
    ) -> GateVerdict {
        let key = format!("{PLUGIN_RULE_PREFIX}{}", escalation.rule_key);
        // A once-only rule (a spend limit) asks whatever the mode, YOLO or
        // earlier approvals say, and is never remembered.
        if !escalation.once_only && (self.bypassing() || self.allow_rules().contains(&key)) {
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
            remember: (!escalation.once_only).then_some(key),
            explanation: None,
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
                    explanation: None,
                })
                .await;
            if verdict != GateVerdict::Allow {
                return verdict;
            }
        }
        GateVerdict::Allow
    }

    /// The decision step, with the plugin observers around it.
    async fn decide(&self, request: ApprovalRequest) -> GateVerdict {
        let Some(hooks) = self.setup.hooks.clone() else {
            let (verdict, choice, by) = self.decide_with(request.clone(), None).await;
            self.audit(&request, choice, by);
            return verdict;
        };
        hooks.before(&request).await;
        let (verdict, choice, by) = self.decide_with(request.clone(), Some(&*hooks)).await;
        self.audit(&request, choice, by);
        hooks.after(&request, choice, by).await;
        verdict
    }

    /// Appends one decision to the audit log, when the gate has one.
    fn audit(&self, request: &ApprovalRequest, choice: &str, by: &str) {
        let Some((log, session)) = &self.audit else {
            return;
        };
        let subject = request
            .subject
            .clone()
            .unwrap_or_else(|| request.tool.clone());
        log.record(
            &Event::new(Kind::Approval, subject, choice)
                .in_session(session.clone())
                .with_detail(format!("by {by}: {}", request.summary())),
        );
    }

    /// The selected plugin transport when there is one, otherwise the
    /// human in an interactive session and the typed mode elsewhere.
    /// Never waits outside interactive sessions. Answers the verdict, the
    /// choice and who made it.
    async fn decide_with(
        &self,
        request: ApprovalRequest,
        hooks: Option<&dyn ApprovalHooks>,
    ) -> (GateVerdict, &'static str, &'static str) {
        if let Some(decided) = self.guarded(&request).await {
            return decided;
        }
        if let Some(hooks) = hooks {
            match self.transport(hooks, &request).await {
                Some(TransportOutcome::Decided(decision)) => {
                    return (
                        self.apply(&request, decision),
                        choice_name(decision),
                        "transport",
                    );
                }
                Some(TransportOutcome::Denied(why)) => {
                    let summary = request.summary();
                    return (
                        GateVerdict::Denied(format!("DENIED: {summary} ({why}). {STOP}")),
                        "deny",
                        "transport",
                    );
                }
                Some(TransportOutcome::Builtin) | None => {}
            }
        }
        let summary = request.summary();
        let context = self.setup.context;
        let mode = match context {
            Context::Interactive => return self.ask(request).await,
            Context::Cron => self.approvals().cron_mode,
            Context::SingleQuery => self.approvals().single_query_mode,
            Context::Unattended => self.approvals().unattended_mode,
        };
        if request.remember.is_none() || mode == UnattendedApproval::Deny {
            self.unobtainable.fetch_add(1, Ordering::Relaxed);
        }
        match (request.remember.is_some(), mode) {
            (false, _) if request.findings.iter().any(|f| f.detector == PROTECTED) => (
                GateVerdict::Denied(format!(
                    "BLOCKED: {summary}. Protected instruction files need a human approval, which {context} runs cannot give."
                )),
                "deny",
                "policy",
            ),
            (false, _) => (
                GateVerdict::Denied(format!(
                    "BLOCKED: {summary}. This call needs a human approval, which {context} runs cannot give. {STOP}"
                )),
                "deny",
                "policy",
            ),
            (true, UnattendedApproval::Approve) => (GateVerdict::Allow, "once", "policy"),
            (true, UnattendedApproval::Deny) => (
                GateVerdict::Denied(format!(
                    "BLOCKED: {summary}. To allow this in {context} runs set approvals.{context}_mode = \"approve\" in config.toml."
                )),
                "deny",
                "policy",
            ),
        }
    }

    /// The guardian's verdict in `smart` mode on a call whose approval can
    /// be remembered (protected files and once-only rules always go to a
    /// human); `None` when it escalates, fails or does not apply.
    async fn guarded(
        &self,
        request: &ApprovalRequest,
    ) -> Option<(GateVerdict, &'static str, &'static str)> {
        let guardian = self.guardian.as_ref()?;
        if self.approvals().mode != ApprovalMode::Smart || request.remember.is_none() {
            return None;
        }
        match guardian.judge(request).await {
            Ok(GuardianVerdict::Approve) => {
                self.breaker.reset();
                Some((GateVerdict::Allow, "once", "guardian"))
            }
            Ok(GuardianVerdict::Deny(reason)) => {
                let threshold = self.approvals().denial_breaker_threshold;
                let mut message = format!(
                    "DENIED: {} (the approval guardian refused it: {reason}). {STOP}",
                    request.summary()
                );
                if let Some(note) = self.breaker.deny(threshold) {
                    message = format!("{message} {note}");
                }
                Some((GateVerdict::Denied(message), "deny", "guardian"))
            }
            Ok(GuardianVerdict::Escalate) => None,
            Err(why) => {
                tracing::warn!(error = %why, "approval guardian unavailable; asking instead");
                None
            }
        }
    }

    /// Asks the selected transport, bound to a fresh request id and the
    /// request's digest; `None` without a transport.
    async fn transport(
        &self,
        hooks: &dyn ApprovalHooks,
        request: &ApprovalRequest,
    ) -> Option<TransportOutcome> {
        let choices = if request.remember.is_some() {
            vec![
                Decision::Once,
                Decision::Session,
                Decision::Always,
                Decision::Deny,
            ]
        } else {
            vec![Decision::Once, Decision::Deny]
        };
        let digest = {
            use sha2::Digest as _;
            let text = serde_json::to_string(request).unwrap_or_default();
            let hash = sha2::Sha256::digest(text.as_bytes());
            let hex = hash.iter().fold(String::new(), |mut hex, b| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{b:02x}");
                hex
            });
            format!("sha256:{hex}")
        };
        let timeout = std::time::Duration::from_secs(self.approvals().timeout_secs);
        let asked = TransportRequest {
            id: format!("apr_{}", ulid::Ulid::generate()),
            digest,
            request: request.clone(),
            choices,
            timeout,
        };
        match tokio::time::timeout(timeout, hooks.transport(asked)).await {
            Ok(outcome) => outcome,
            Err(_) => Some(TransportOutcome::Denied(format!(
                "the approval transport gave no answer within {} s (approvals.timeout_secs)",
                timeout.as_secs()
            ))),
        }
    }

    /// Applies a decision made by a transport (as a human's would be).
    fn apply(&self, request: &ApprovalRequest, decision: Decision) -> GateVerdict {
        if decision != Decision::Deny {
            self.breaker.reset();
        }
        let summary = request.summary();
        match decision {
            Decision::Once => GateVerdict::Allow,
            Decision::Session => {
                self.lock().extend(request.remember.clone());
                GateVerdict::Allow
            }
            Decision::Always => {
                if let Some(pattern) = &request.remember {
                    self.remember_always(pattern);
                }
                GateVerdict::Allow
            }
            Decision::Deny | Decision::Explain => GateVerdict::Denied(format!(
                "DENIED: {summary} (denied through the approval transport). {STOP}"
            )),
        }
    }

    async fn ask(&self, request: ApprovalRequest) -> (GateVerdict, &'static str, &'static str) {
        let summary = request.summary();
        let remember = request.remember.clone();
        let denied = |why: &str| GateVerdict::Denied(format!("DENIED: {summary} ({why}). {STOP}"));
        let answer = self.ask_explaining(request).await;
        if matches!(
            answer,
            Ok(Decision::Once | Decision::Session | Decision::Always)
        ) {
            self.breaker.reset();
        }
        match answer {
            Ok(Decision::Once) => (GateVerdict::Allow, "once", "user"),
            Ok(Decision::Session) => {
                self.lock().extend(remember);
                (GateVerdict::Allow, "session", "user")
            }
            Ok(Decision::Always) => {
                if let Some(pattern) = remember {
                    self.remember_always(&pattern);
                }
                (GateVerdict::Allow, "always", "user")
            }
            Ok(Decision::Deny | Decision::Explain) => {
                (denied("denied by the user"), "deny", "user")
            }
            Err(Unanswered::TimedOut(after)) => (
                denied(&format!(
                    "no answer within {} s (approvals.timeout_secs)",
                    after.as_secs()
                )),
                "deny",
                "timeout",
            ),
            Err(other) => (denied(&other.to_string()), "deny", "user"),
        }
    }

    /// Asks the human; an `explain` answer adds the explanation and asks
    /// again (the command never runs on it), at most [`EXPLAINS`] times.
    /// Asks this gate's human channel about a request raised elsewhere (a
    /// delegated child escalating to its root session's surface; spec:
    /// subagent-delegation, Child approvals fail closed). Without a human
    /// channel the answer is `Unanswered`, which callers deny.
    ///
    /// # Errors
    ///
    /// [`Unanswered`] when nobody answered.
    pub async fn escalate(&self, request: ApprovalRequest) -> Result<Decision, Unanswered> {
        self.ask_explaining(request).await
    }

    async fn ask_explaining(&self, mut request: ApprovalRequest) -> Result<Decision, Unanswered> {
        for _ in 0..EXPLAINS {
            match self.setup.channel.ask(request.clone()).await {
                Ok(Decision::Explain) => {
                    request.explanation = Some(self.explain(&request).await);
                }
                other => return other,
            }
        }
        Ok(Decision::Deny)
    }

    /// The explanation of `request`'s command (or, for a call without
    /// one, of its findings).
    async fn explain(&self, request: &ApprovalRequest) -> String {
        let Some(line) = request
            .subject
            .as_deref()
            .filter(|_| request.remember.is_some())
        else {
            return format!("Explanation: {}", request.summary());
        };
        let cwd = std::path::PathBuf::from(&self.setup.floors.cwd);
        super::explain::explain(line, &request.findings, &cwd, self.previewer.as_ref()).await
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
            let rules = request
                .escalation
                .into_iter()
                .chain(request.guarded)
                .chain(request.tool.descriptor.approval.as_ref());
            for escalation in rules {
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

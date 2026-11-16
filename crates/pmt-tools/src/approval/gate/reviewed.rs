//! The gate's file-edit approvals (spec: tui, Inline diff review for file
//! edits; acp-adapter, Edit-approval modes): writes to protected
//! instruction files, and an editor session's edits, ask with the change
//! as a diff. An approval that keeps only some of its hunks, or that
//! edited the content, rewrites the call so the tool writes exactly that,
//! and the tool's result tells the model what was applied.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::super::Finding;
use super::super::edits::{FILE_EDIT, previews};
use super::super::review::revise;
use super::{ApprovalRequest, CommandGate, EditDiff, PROTECTED, STOP, ssh_config, written_paths};
use crate::dispatch::{GateRequest, GateVerdict};
use crate::files::guards::{absolute, device_namespace};

/// A call's arguments as reviews revised them, and what the model is told.
pub(super) struct Reviewed {
    arguments: Value,
    notes: Vec<String>,
}

impl Reviewed {
    /// Nothing reviewed yet.
    pub(super) fn new(arguments: &Value) -> Self {
        Self {
            arguments: arguments.clone(),
            notes: Vec::new(),
        }
    }

    /// The verdict on a call every check allowed.
    pub(super) fn verdict(self) -> GateVerdict {
        if self.notes.is_empty() {
            GateVerdict::Allow
        } else {
            GateVerdict::Revised {
                arguments: self.arguments,
                note: self.notes.join("\n"),
            }
        }
    }
}

impl CommandGate {
    /// An argument path as the absolute path diffs carry.
    fn resolve(&self, path: &str) -> PathBuf {
        let floors = &self.setup.floors;
        let user_home = floors.user_home.as_deref().map(Path::new);
        absolute(path, Path::new(&floors.cwd), user_home)
    }

    /// The diff of the call's write to `path` (absolute), when the tool's
    /// change can be previewed.
    fn diff_of(&self, tool: &str, arguments: &Value, path: &Path) -> Option<EditDiff> {
        let resolve = |p: &str| self.resolve(p);
        previews(tool, arguments, &resolve)
            .into_iter()
            .find(|d| Path::new(&d.path) == path)
    }

    /// Asks about one edit; an answer that reviewed its content revises
    /// `reviewed`'s arguments.
    async fn ask_edit(
        &self,
        tool: &str,
        request: ApprovalRequest,
        reviewed: &mut Reviewed,
    ) -> GateVerdict {
        let diff = request.diff.clone();
        let (verdict, review) = self.decide_reviewed(request).await;
        let Some(diff) = diff.filter(|_| verdict == GateVerdict::Allow && review.revises()) else {
            return verdict;
        };
        let resolve = |p: &str| self.resolve(p);
        match revise(tool, &reviewed.arguments, &diff, &review, &resolve) {
            Ok(revision) => {
                reviewed.arguments = revision.arguments;
                reviewed.notes.push(revision.note);
                GateVerdict::Allow
            }
            Err(why) => GateVerdict::Denied(format!("BLOCKED: {why}. {STOP}")),
        }
    }

    /// An editor session's edit policy: each edit it asks about becomes an
    /// approval request carrying the diff, answered once (never
    /// remembered).
    pub(super) async fn check_edits(
        &self,
        request: &GateRequest<'_>,
        reviewed: &mut Reviewed,
    ) -> GateVerdict {
        let Some(policy) = self.edit_policy() else {
            return GateVerdict::Allow;
        };
        if self.bypassing() {
            return GateVerdict::Allow;
        }
        let cwd = PathBuf::from(&self.setup.floors.cwd);
        let resolve = |p: &str| self.resolve(p);
        let tool = request.tool.name();
        for diff in previews(tool, &reviewed.arguments.clone(), &resolve) {
            if !policy.asks(Path::new(&diff.path), &cwd) {
                continue;
            }
            let asked = ApprovalRequest {
                tool: tool.to_owned(),
                call_id: request.call_id.to_owned(),
                subject: Some(diff.path.clone()),
                findings: vec![Finding {
                    detector: FILE_EDIT.to_owned(),
                    reason: "this editor session asks before file edits".to_owned(),
                    fragment: diff.path.clone(),
                }],
                remember: None,
                explanation: None,
                diff: Some(diff),
            };
            let verdict = self.ask_edit(tool, asked, reviewed).await;
            if verdict != GateVerdict::Allow {
                return verdict;
            }
        }
        GateVerdict::Allow
    }

    /// Writes to protected instruction files ask in every mode, showing
    /// the change as a diff when the tool's edit can be previewed. A
    /// written device-namespace or UNC path is refused here, before the
    /// protected check resolves it (which would open it on Windows), in
    /// every mode.
    pub(super) async fn check_protected(
        &self,
        request: &GateRequest<'_>,
        reviewed: &mut Reviewed,
    ) -> GateVerdict {
        let user_home = self.setup.floors.user_home.as_deref().map(Path::new);
        let tool = request.tool.name();
        for path in written_paths(request.tool, request.arguments) {
            let absolute = self.resolve(&path);
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
            let asked = ApprovalRequest {
                tool: tool.to_owned(),
                call_id: request.call_id.to_owned(),
                subject: Some(path.clone()),
                findings: vec![Finding {
                    detector: PROTECTED.to_owned(),
                    reason,
                    fragment: path,
                }],
                remember: None,
                explanation: None,
                diff: self.diff_of(tool, &reviewed.arguments, &absolute),
            };
            let verdict = self.ask_edit(tool, asked, reviewed).await;
            if verdict != GateVerdict::Allow {
                return verdict;
            }
        }
        GateVerdict::Allow
    }
}

//! `prometheus approvals test` (spec: tool-system, Approval dry-run test
//! command): a command line through the real gate order without running
//! it, asking anyone, calling a guardian or writing anything. Every
//! verdict exits 0; a command line that cannot be read exits 2.

use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_config::grants::GrantsFile;
use pmt_config::schema::{ApprovalMode, ApprovalsConfig, UnattendedApproval};
use pmt_core::error::{Error, codes};
use pmt_tools::approval::{Context as GateContext, Finding, Floors, Policy, Verdict, evaluate};
use serde::Serialize;

use crate::cli::{ApprovalContextArg, ApprovalModeArg, ApprovalsCommand, ApprovalsTestArgs};
use crate::context::Context;
use crate::output::{document, print_raw};

/// Runs one `approvals` subcommand.
///
/// # Errors
///
/// `approvals.unparsable` (exit 2); profile and config errors.
pub fn run(command: &ApprovalsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        ApprovalsCommand::Test(args) => test(args, ctx),
    }
}

/// The `approvals test` document.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The command line tested.
    pub command: String,
    /// `hardline`, `deny rule`, `bypass`, `allowlisted`, `would prompt` or
    /// `allowed`.
    pub verdict: &'static str,
    /// The matching hardline reason, deny rule or allow rule.
    pub rule: Option<String>,
    /// Detectors that would ask (for `would prompt`).
    pub findings: Vec<Finding>,
    /// What would happen to the call in this mode and context.
    pub outcome: String,
    /// The mode assumed.
    pub mode: ApprovalMode,
    /// The context assumed.
    pub context: GateContext,
    /// The backend assumed.
    pub backend: String,
    /// Always false: nothing runs.
    pub executed: bool,
}

fn test(args: &ApprovalsTestArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let mode = args.mode.map_or(config.approvals.mode, mode);
    let context = context(args.context);
    let line = args.command.join(" ");
    let allow = GrantsFile::of(scope.profile()).load()?.allow;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let binary = std::env::current_exe().ok();
    let floors = Floors::new(&cwd)
        .user_home(ctx.launch.user_home().map(PathBuf::as_path))
        .prometheus_home(scope.home())
        .binary(binary.as_deref());
    let policy = Policy {
        bypass: mode == ApprovalMode::Off,
        deny: &config.approvals.deny,
        allow: &allow,
        floors: &floors,
    };
    let verdict = evaluate(&line, &policy).map_err(|err| {
        Error::new(
            &codes::APPROVALS_UNPARSABLE,
            format!("cannot read the command: {err}"),
        )
    })?;
    let report = Report {
        outcome: outcome(&verdict, context, &config.approvals),
        command: line,
        verdict: verdict.label(),
        rule: match &verdict {
            Verdict::Hardline(rule) | Verdict::DenyRule(rule) | Verdict::Allowlisted(rule) => {
                Some(rule.clone())
            }
            _ => None,
        },
        findings: match verdict {
            Verdict::Prompt(findings) => findings,
            _ => Vec::new(),
        },
        mode,
        context,
        backend: args
            .backend
            .clone()
            .unwrap_or_else(|| config.terminal.backend.clone()),
        executed: false,
    };
    if ctx.json {
        return document("approvals test", &report);
    }
    print_raw(&render(&report))
}

fn mode(arg: ApprovalModeArg) -> ApprovalMode {
    match arg {
        ApprovalModeArg::Manual => ApprovalMode::Manual,
        ApprovalModeArg::Smart => ApprovalMode::Smart,
        ApprovalModeArg::Off => ApprovalMode::Off,
    }
}

fn context(arg: ApprovalContextArg) -> GateContext {
    match arg {
        ApprovalContextArg::Interactive => GateContext::Interactive,
        ApprovalContextArg::Cron => GateContext::Cron,
        ApprovalContextArg::SingleQuery => GateContext::SingleQuery,
        ApprovalContextArg::Unattended => GateContext::Unattended,
    }
}

/// What the gate would do with the call.
fn outcome(verdict: &Verdict, context: GateContext, approvals: &ApprovalsConfig) -> String {
    if let Some(message) = verdict.blocked_message() {
        return format!("blocked: {message}");
    }
    let Verdict::Prompt(findings) = verdict else {
        return "runs without asking".to_owned();
    };
    let unattended = match context {
        GateContext::Interactive => {
            return format!(
                "asks the human (once, session, always or deny); no answer within {} s denies",
                approvals.timeout_secs
            );
        }
        GateContext::Cron => approvals.cron_mode,
        GateContext::SingleQuery => approvals.single_query_mode,
        GateContext::Unattended => approvals.unattended_mode,
    };
    match unattended {
        UnattendedApproval::Approve => {
            format!("runs without asking (approvals.{context}_mode = \"approve\")")
        }
        UnattendedApproval::Deny => {
            let summary: Vec<String> = findings.iter().map(ToString::to_string).collect();
            format!(
                "denied without waiting: BLOCKED: {}. To allow this in {context} runs set approvals.{context}_mode = \"approve\" in config.toml.",
                summary.join("; ")
            )
        }
    }
}

fn render(report: &Report) -> String {
    let mut out = format!("verdict: {}\n", report.verdict);
    if let Some(rule) = &report.rule {
        let _ = writeln!(out, "rule: {rule}");
    }
    for finding in &report.findings {
        let _ = writeln!(out, "detector: {finding}");
    }
    let _ = writeln!(out, "outcome: {}", report.outcome);
    let mode = match report.mode {
        ApprovalMode::Manual => "manual",
        ApprovalMode::Smart => "smart",
        ApprovalMode::Off => "off",
    };
    let _ = writeln!(
        out,
        "mode: {mode}, context: {}, backend: {}",
        report.context, report.backend
    );
    out.push_str("nothing was executed\n");
    out
}

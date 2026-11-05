//! `prometheus security audit verify` (spec: security-boundaries,
//! Tamper-evident security audit log): walks the hash chain of the
//! selected profile's `logs/security-audit.jsonl`; exit 0 when it is
//! intact, 1 naming the first line that is not.

use pmt_core::audit::{AuditLog, Event, Kind, Verdict};
use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::cli::{AuditCommand, SecurityCommand};
use crate::context::Context;
use crate::output::{document, print_raw};

/// Runs one `security` subcommand and returns its exit code.
///
/// # Errors
///
/// The profile cannot be opened or the log cannot be read.
pub fn run(command: &SecurityCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    match command {
        SecurityCommand::Audit {
            command: AuditCommand::Verify,
        } => verify(ctx),
    }
}

#[derive(Serialize)]
struct Report {
    path: String,
    intact: bool,
    records: Option<usize>,
    broken_line: Option<usize>,
    reason: Option<String>,
}

fn verify(ctx: &Context<'_>) -> Result<u8, Error> {
    let profile = ctx.profile_dir()?;
    let log = AuditLog::of(profile.path());
    let verdict = log
        .verify()
        .map_err(|e| Error::io(&codes::HOME_IO, log.path(), &e))?;
    let path = log.path().display().to_string();
    let report = match verdict {
        Verdict::Intact { records } => Report {
            path,
            intact: true,
            records: Some(records),
            broken_line: None,
            reason: None,
        },
        Verdict::Broken { line, reason } => Report {
            path,
            intact: false,
            records: None,
            broken_line: Some(line),
            reason: Some(reason),
        },
    };
    let code = u8::from(!report.intact);
    if ctx.json {
        document("security audit verify", &report)?;
    } else if let Some(line) = report.broken_line {
        print_raw(&format!(
            "audit log broken at line {line}: {} ({})\n",
            report.reason.as_deref().unwrap_or_default(),
            report.path
        ))?;
    } else {
        print_raw(&format!(
            "audit log intact: {} records ({})\n",
            report.records.unwrap_or_default(),
            report.path
        ))?;
    }
    Ok(code)
}

/// Records `result`'s failure in the selected profile's security audit log
/// when it is a signature or digest that did not verify, or a missing
/// signature the policy refuses (a plugin package, a release artifact);
/// passes `result` through.
pub(crate) fn audit_signature<T>(
    ctx: &Context<'_>,
    subject: &str,
    result: Result<T, Error>,
) -> Result<T, Error> {
    if let Err(err) = &result
        && [
            &codes::PLUGINS_SIGNATURE,
            &codes::PLUGINS_UNSIGNED,
            &codes::UPDATE_SIGNATURE,
        ]
        .contains(&err.code())
        && let Ok(profile) = ctx.profile_dir()
    {
        AuditLog::of(profile.path()).record(
            &Event::new(Kind::Signature, subject, "refused").with_detail(err.message().to_owned()),
        );
    }
    result
}

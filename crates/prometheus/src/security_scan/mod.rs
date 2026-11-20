//! `prometheus security scan` (spec: security-boundaries, Dependency
//! vulnerability scan): the exact versions of the managed runtimes,
//! Python-bridge environments, plugin dependency manifests and pinned
//! `npx`/`uvx` MCP server packages, checked against the OSV database
//! (`security.scan.osv_url`). The network is contacted only by this
//! command. Exit 1 when a finding reaches `--fail-on` (default
//! `critical`), 0 otherwise, 69 when OSV cannot be reached: a CI gate.

mod components;
mod osv;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use clap::Args;
use pmt_core::error::Error;
use serde_json::json;

pub use osv::Severity;

use crate::context::Context;
use crate::output::{document, print_raw};

/// `prometheus security scan`.
#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Exit 1 when any finding is at least this severe.
    #[arg(long, value_enum, default_value = "critical")]
    pub fail_on: Severity,
    /// Leave out the managed runtimes.
    #[arg(long)]
    pub skip_runtimes: bool,
    /// Leave out plugin manifests and Python-bridge environments.
    #[arg(long)]
    pub skip_plugins: bool,
    /// Leave out MCP server packages.
    #[arg(long)]
    pub skip_mcp: bool,
}

/// Runs the scan.
///
/// # Errors
///
/// `security.osv_unreachable` (69).
pub fn run(args: &ScanArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let packages = components::collect(
        &scope,
        components::Skip {
            runtimes: args.skip_runtimes,
            plugins: args.skip_plugins,
            mcp: args.skip_mcp,
        },
    );
    let base = scope.config().security.scan.osv_url.clone();
    let findings = crate::model_cmd::block_on(osv::findings(&base, &packages))??;
    let failed = findings.iter().any(|f| f.severity >= args.fail_on);
    if ctx.json {
        document(
            "security scan",
            &json!({
                "packages": packages.len(),
                "fail_on": args.fail_on.as_str(),
                "failed": failed,
                "findings": findings,
            }),
        )?;
    } else {
        print_raw(&report(packages.len(), &findings))?;
    }
    Ok(u8::from(failed))
}

/// Findings grouped by component.
fn report(checked: usize, findings: &[osv::Finding]) -> String {
    let mut groups: BTreeMap<&str, Vec<&osv::Finding>> = BTreeMap::new();
    for finding in findings {
        groups.entry(&finding.component).or_default().push(finding);
    }
    let mut out = format!(
        "checked {checked} package version(s): {} finding(s)\n",
        findings.len()
    );
    for (component, list) in groups {
        let _ = writeln!(out, "{component}");
        for f in list {
            let _ = writeln!(
                out,
                "  {} {}@{}  {}  fixed in {}  {}",
                f.id,
                f.package,
                f.version,
                f.severity.as_str(),
                f.fixed.as_deref().unwrap_or("(no fix yet)"),
                f.summary
            );
        }
    }
    out
}

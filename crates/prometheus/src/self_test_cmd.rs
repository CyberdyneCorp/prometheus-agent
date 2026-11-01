//! `prometheus self-test` (hidden): what `prometheus update` runs on a
//! staged build before it may replace the installed one (spec:
//! distribution-and-update, Atomic binary swap with health check): this
//! build's version and embedded schema versions, a sandbox probe, and every
//! profile's `config.toml` checked read-only (migrated in memory only, so a
//! staged build never changes the home it is tested against).
//!
//! Prints the report as JSON with `--json`; exits 0 when every check
//! passes, else 1 naming the first failure on stderr.

use std::path::{Path, PathBuf};

use pmt_config::home;
use pmt_config::schema::SandboxConfig;
use pmt_sandbox::{Paths, Policy, Sandbox};
use serde::Serialize;

use crate::context::Context;
use crate::version::VersionInfo;

/// One profile's config check.
#[derive(Debug, Serialize)]
struct ProfileCheck {
    name: String,
    ok: bool,
    error: Option<String>,
}

/// The `self-test --json` report; `version` and the schema versions use
/// the same keys as `version --json`.
#[derive(Debug, Serialize)]
struct Report {
    version: &'static str,
    target: &'static str,
    config_schema_version: u32,
    store_schema_version: u32,
    trajectory_grammar: u32,
    /// Enforcement level, `None` when the probe failed.
    sandbox: Option<String>,
    sandbox_error: Option<String>,
    profiles: Vec<ProfileCheck>,
}

/// Runs the self-test.
#[must_use]
pub fn run(ctx: &Context<'_>) -> u8 {
    let info = VersionInfo::current();
    let root = home::resolve(ctx.home_flag, ctx.launch).ok();
    let user_home = ctx
        .launch
        .user_home()
        .cloned()
        .unwrap_or_else(std::env::temp_dir);
    let sandbox = probe_sandbox(user_home, root.as_deref());
    let report = Report {
        version: info.version,
        target: info.target,
        config_schema_version: info.config_schema_version,
        store_schema_version: info.store_schema_version,
        trajectory_grammar: info.trajectory_grammar,
        sandbox: sandbox.as_ref().ok().cloned(),
        sandbox_error: sandbox.err(),
        profiles: root.as_deref().map(check_profiles).unwrap_or_default(),
    };
    let failure = report
        .sandbox_error
        .as_ref()
        .map(|e| format!("sandbox probe: {e}"))
        .or_else(|| {
            report.profiles.iter().find(|p| !p.ok).map(|p| {
                format!(
                    "profile {}: {}",
                    p.name,
                    p.error.as_deref().unwrap_or("invalid")
                )
            })
        });
    if ctx.json {
        let _ = crate::output::json(&report);
    } else {
        println!("prometheus {} self-test", report.version);
    }
    match failure {
        None => pmt_core::exit::SUCCESS,
        Some(reason) => {
            eprintln!("{reason}");
            1
        }
    }
}

/// Builds the baseline policy and probes the OS sandbox; reports the
/// enforcement level (`full`, `partial:...` or `none`).
fn probe_sandbox(user_home: PathBuf, home: Option<&Path>) -> Result<String, String> {
    let scratch = std::env::temp_dir();
    let paths = Paths {
        user_home,
        prometheus_home: home.map_or_else(|| scratch.clone(), Path::to_path_buf),
        workspace: scratch.clone(),
        scratch: vec![scratch],
    };
    let policy = Policy::baseline(&paths).map_err(|e| e.to_string())?;
    let sandbox = Sandbox::new(policy, &SandboxConfig::default()).map_err(|e| e.to_string())?;
    Ok(sandbox.report().enforcement().to_string())
}

/// Every profile directory under `<home>/profiles/`, without creating
/// anything.
fn check_profiles(root: &Path) -> Vec<ProfileCheck> {
    let Ok(entries) = std::fs::read_dir(root.join("profiles")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let checked =
                pmt_config::check_readonly(&root.join("profiles").join(&name).join("config.toml"));
            ProfileCheck {
                name,
                ok: checked.is_ok(),
                error: checked.err().map(|e| e.to_string()),
            }
        })
        .collect()
}

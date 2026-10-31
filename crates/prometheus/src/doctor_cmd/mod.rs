//! `prometheus doctor [<group>] [--fix] [--online] [--strict] [--json]
//! [--all-profiles]` (spec: workspace-architecture, Doctor; Doctor
//! repairs are explicit and enumerated; cli, Doctor; security-boundaries,
//! sandbox enforcement report). Each group returns `ok` / `warn` / `fail`
//! checks with a hint; the command prints one line per check and exits 1
//! when any check fails (`--strict`: any warning too; `doctor security
//! --strict` exits 77 on a downgrade). Without `--online` nothing touches
//! the network.

mod fix;
mod groups;
mod system;

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_core::doctor::{Check, Status};
use pmt_core::error::{Error, codes};
use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::context::Context;
use crate::output::{document, print_raw};

/// The groups, in the order they run.
pub const GROUPS: &[&str] = &[
    "home",
    "config",
    "secrets",
    "store",
    "daemon",
    "sandbox",
    "terminal",
    "runtimes",
    "plugins",
    "observability",
    "network",
    "providers",
    "security",
];

/// Exit status of `doctor security --strict` with a downgrade.
pub const STRICT_SECURITY: u8 = 77;

/// What the checks of one run need.
pub(crate) struct Inputs<'a> {
    pub ctx: &'a Context<'a>,
    pub scope: &'a ProfileScope,
    pub online: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    profile: &'a str,
    repairs: &'a [String],
    checks: &'a [Check],
    failed: bool,
}

/// Runs `prometheus doctor`.
///
/// # Errors
///
/// `doctor.unknown_group` (2); home errors.
pub fn run(args: &DoctorArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    if let Some(group) = &args.group
        && !GROUPS.contains(&group.as_str())
    {
        return Err(Error::new(
            &codes::DOCTOR_UNKNOWN_GROUP,
            format!(
                "unknown doctor group '{group}'; groups: {}",
                GROUPS.join(", ")
            ),
        ));
    }
    let profiles: Vec<Option<String>> = if args.all_profiles {
        groups::profile_names(&pmt_config::home::resolve(ctx.home_flag, ctx.launch)?)
            .into_iter()
            .map(Some)
            .collect()
    } else {
        vec![ctx.profile.map(str::to_owned)]
    };
    let mut status = 0;
    for profile in &profiles {
        let ctx = Context {
            profile: profile.as_deref(),
            ..*ctx
        };
        status = status.max(run_profile(args, &ctx)?);
    }
    Ok(status)
}

fn run_profile(args: &DoctorArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    // Permissions are read (and repaired) before the home is opened:
    // opening it tightens them on its own.
    let root = pmt_config::home::resolve(ctx.home_flag, ctx.launch)?;
    let name = groups::raw_profile(&root, ctx.profile);
    let profile = root.join("profiles").join(&name);
    let selected = |group: &str| args.group.as_deref().is_none_or(|g| g == group);
    let repairs = if args.fix {
        fix::run(ctx, &root, &profile)?
    } else {
        Vec::new()
    };
    let (home_checks, secrets_mode) = groups::permissions(&root, &profile);
    let mut checks = Vec::new();
    if selected("home") {
        checks.extend(home_checks);
    }
    match ctx.scope() {
        Ok(scope) => {
            let inputs = Inputs {
                ctx,
                scope: &scope,
                online: args.online,
            };
            for group in GROUPS.iter().filter(|g| **g != "home" && selected(g)) {
                if *group == "secrets" {
                    checks.push(secrets_mode.clone());
                }
                checks.extend(crate::model_cmd::block_on(run_group(group, &inputs))??);
            }
        }
        Err(err) => checks.push(
            Check::new("config", "valid", Status::Fail, err.message().to_owned()).with_hint(
                err.hint()
                    .unwrap_or("fix config.toml: prometheus config validate")
                    .to_owned(),
            ),
        ),
    }
    finish(args, ctx, &name, &repairs, &checks)
}

fn finish(
    args: &DoctorArgs,
    ctx: &Context<'_>,
    name: &str,
    repairs: &[String],
    checks: &[Check],
) -> Result<u8, Error> {
    let failed = checks
        .iter()
        .any(|c| c.status == Status::Fail || (args.strict && c.status == Status::Warn));
    print(ctx, name, repairs, checks, failed)?;
    let downgraded = checks
        .iter()
        .any(|c| c.group == "security" && c.status != Status::Ok);
    Ok(
        if args.strict && args.group.as_deref() == Some("security") && downgraded {
            STRICT_SECURITY
        } else {
            u8::from(failed)
        },
    )
}

async fn run_group(group: &str, inputs: &Inputs<'_>) -> Result<Vec<Check>, Error> {
    Ok(match group {
        "config" => groups::config(inputs),
        "secrets" => groups::secrets(inputs)?,
        "store" => groups::store(inputs),
        "daemon" => groups::daemon(inputs),
        "sandbox" => system::sandbox(inputs),
        "terminal" => system::terminal(inputs),
        "runtimes" => groups::runtimes(),
        "plugins" => groups::plugins(),
        "observability" => groups::observability(inputs),
        "network" => system::network(inputs).await,
        "providers" => groups::providers(inputs).await?,
        "security" => system::security(inputs),
        _ => Vec::new(),
    })
}

fn print(
    ctx: &Context<'_>,
    profile: &str,
    repairs: &[String],
    checks: &[Check],
    failed: bool,
) -> Result<u8, Error> {
    if ctx.json {
        return document(
            "doctor",
            &Report {
                profile,
                repairs,
                checks,
                failed,
            },
        );
    }
    let mut text = String::new();
    for repair in repairs {
        let _ = writeln!(text, "fixed {repair}");
    }
    let _ = writeln!(text, "profile {profile}");
    for check in checks {
        let status = match check.status {
            Status::Ok => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "fail",
        };
        let _ = write!(text, "{status} {:<13} {}", check.group, check.message);
        if let Some(hint) = &check.hint
            && check.status != Status::Ok
        {
            let _ = write!(text, " (hint: {hint})");
        }
        text.push('\n');
    }
    print_raw(&text)
}

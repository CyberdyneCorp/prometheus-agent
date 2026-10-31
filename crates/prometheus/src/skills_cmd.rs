//! `prometheus skills trust|eject|diff|reset|doctor` (spec: skills,
//! Project-local skills require explicit trust; Bundled skills are
//! read-only and ejectable; SKILL.md format: problems are reported by
//! `skills doctor`). The listing, show, enable/disable and lint commands
//! arrive with the rest of the skills CLI.

use std::fmt::Write as _;

use pmt_core::error::Error;
use pmt_skills::bundled::Bundled;
use pmt_skills::{activation, eject};
use serde_json::json;

use crate::cli::SkillsCommand;
use crate::context::Context;
use crate::output::{json, print_raw};

/// Runs one `skills` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &SkillsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let user_home = ctx.launch.user_home().map(std::path::PathBuf::as_path);
    let bundled = Bundled::builtin();
    match command {
        SkillsCommand::Trust { root } => {
            let message = activation::trust(scope.profile(), root)?;
            if ctx.json {
                return json(
                    &json!({"profile": scope.name().as_str(), "trusted": root.canonicalize().ok()}),
                );
            }
            eprintln!("{message}");
            Ok(0)
        }
        SkillsCommand::Eject { name } => {
            let catalog = eject::profile_catalog(&scope, None, user_home, bundled);
            let ejected = eject::eject(scope.dir(), &catalog, name)?;
            if ctx.json {
                return json(&json!({"name": ejected.name, "path": ejected.path}));
            }
            eprintln!("ejected '{}' to {}", ejected.name, ejected.path.display());
            Ok(0)
        }
        SkillsCommand::Diff { name } => {
            let catalog = eject::profile_catalog(&scope, None, user_home, bundled.clone());
            print_raw(&eject::diff(scope.dir(), &catalog, &bundled, name)?)
        }
        SkillsCommand::Doctor => doctor(ctx, &scope, user_home, bundled),
        SkillsCommand::Reset { name } => {
            let catalog = eject::profile_catalog(&scope, None, user_home, bundled);
            eject::reset(scope.dir(), &catalog, name)?;
            if ctx.json {
                return json(&json!({"name": name, "reset": true}));
            }
            eprintln!("'{name}' now resolves to the bundled skill");
            Ok(0)
        }
    }
}

/// Prints every `SKILL.md` that did not load (`<path>:<line>: <reason>`)
/// and every quarantined project skill; exit 1 when there is any.
fn doctor(
    ctx: &Context<'_>,
    scope: &pmt_config::ProfileScope,
    user_home: Option<&std::path::Path>,
    bundled: std::sync::Arc<Bundled>,
) -> Result<u8, Error> {
    let cwd = std::env::current_dir().ok();
    let catalog = eject::profile_catalog(scope, cwd.as_deref(), user_home, bundled);
    let problems: Vec<_> = catalog
        .problems
        .iter()
        .map(|p| json!({"path": p.path, "line": p.line, "reason": p.reason}))
        .chain(catalog.quarantined.iter().map(|(skill, why)| {
            json!({"path": skill.dir.display(), "line": 0, "reason": format!("quarantined: {why}")})
        }))
        .collect();
    let code = u8::from(!problems.is_empty());
    if ctx.json {
        json(
            &json!({"profile": scope.name().as_str(), "problems": problems, "notices": catalog.notices}),
        )?;
        return Ok(code);
    }
    let mut text = String::new();
    for p in &problems {
        let line = p["line"].as_u64().unwrap_or(0);
        let path = p["path"].as_str().unwrap_or_default();
        let reason = p["reason"].as_str().unwrap_or_default();
        if line > 0 {
            let _ = writeln!(text, "{path}:{line}: {reason}");
        } else {
            let _ = writeln!(text, "{path}: {reason}");
        }
    }
    for notice in &catalog.notices {
        let _ = writeln!(text, "{notice}");
    }
    if text.is_empty() {
        text.push_str("no skill problems\n");
    }
    print_raw(&text)?;
    Ok(code)
}
